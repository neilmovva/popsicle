use anyhow::Context;
use async_std::{fs::File, prelude::*};
use srmw::*;
use std::{collections::HashMap, io::SeekFrom, time::Instant};

use crate::ImageReader;

pub trait Progress {
    type Device;
    fn message(&mut self, device: &Self::Device, kind: &str, message: &str);
    fn finish(&mut self);
    fn set(&mut self, value: u64);
}

async fn sync_device_file(file: &File) -> std::io::Result<()> {
    file.sync_all().await?;

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::{fs::FileTypeExt, io::AsRawFd};

        let file_type = file.metadata().await?.file_type();
        if file_type.is_block_device() || file_type.is_char_device() {
            synchronize_macos_disk(file.as_raw_fd())?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct DkSynchronize {
    offset: u64,
    length: u64,
    options: u32,
    reserved: [u8; 4],
}

#[cfg(target_os = "macos")]
const DKIOCSYNCHRONIZE: libc::c_ulong = 0x8018_6416;

#[cfg(target_os = "macos")]
fn synchronize_macos_disk(descriptor: std::os::unix::io::RawFd) -> std::io::Result<()> {
    let request = DkSynchronize::default();
    loop {
        let result = unsafe { libc::ioctl(descriptor, DKIOCSYNCHRONIZE, &request) };
        if result != -1 {
            return Ok(());
        }
        let why = std::io::Error::last_os_error();
        if why.kind() != std::io::ErrorKind::Interrupted {
            return Err(why);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor;
    use std::{fs, io::Write};

    #[derive(Default)]
    struct TestProgress;

    impl Progress for TestProgress {
        type Device = ();
        fn message(&mut self, _: &(), _: &str, _: &str) {}
        fn finish(&mut self) {}
        fn set(&mut self, _: u64) {}
    }

    #[test]
    fn streams_zstandard_and_rewinds_for_verification() {
        let stem = format!("popsicle-task-test-{}", std::process::id());
        let source_path = std::env::temp_dir().join(format!("{}-source", stem));
        let destination_path = std::env::temp_dir().join(format!("{}-destination", stem));
        let expected = b"decoded bytes are streamed directly to the destination";
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(expected).unwrap();
        fs::write(&source_path, encoder.finish().unwrap()).unwrap();

        executor::block_on(async {
            let source = std::fs::File::open(&source_path).unwrap();
            let mut image = ImageReader::new(source).unwrap();
            let size = image.inspect().await.unwrap();
            let destination = async_std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&destination_path)
                .await
                .unwrap();
            let mut task = Task::new(image, true, size);
            task.subscribe(destination, (), TestProgress);
            task.process(&mut [0; 7]).await.unwrap();
        });

        assert_eq!(fs::read(&destination_path).unwrap(), expected);
        fs::remove_file(source_path).unwrap();
        fs::remove_file(destination_path).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_synchronize_request_matches_the_sdk() {
        assert_eq!(std::mem::size_of::<DkSynchronize>(), 24);
        assert_eq!(DKIOCSYNCHRONIZE, 0x8018_6416);
        assert_eq!(DkSynchronize::default().options, 0);
    }
}

#[derive(new)]
pub struct Task<P: Progress> {
    image: ImageReader,

    #[new(default)]
    pub writer: MultiWriter<File>,

    #[new(default)]
    pub state: HashMap<usize, (P::Device, P)>,

    #[new(value = "125")]
    pub millis_between: u64,

    check: bool,

    expected_size: u64,
}

impl<P: Progress> Task<P> {
    /// Performs the asynchronous USB device flashing.
    pub async fn process(mut self, buf: &mut [u8]) -> anyhow::Result<()> {
        self.image.rewind().await.context("failed to rewind image")?;
        self.copy(buf).await.context("failed to copy image")?;
        self.sync().await.context("failed to synchronize devices")?;

        if self.check {
            self.seek().await.context("failed to seek devices to start")?;
            self.validate(buf).await.context("validation error")?;
        }

        for (_, pb) in self.state.values_mut() {
            pb.finish();
        }

        Ok(())
    }

    pub fn subscribe(&mut self, file: File, device: P::Device, progress: P) -> &mut Self {
        let entity = self.writer.insert(file);
        self.state.insert(entity, (device, progress));
        self
    }

    async fn copy(&mut self, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut total = 0u64;
        let mut last = Instant::now();
        {
            let mut reader = (&mut self.image).take(self.expected_size);
            let mut stream = self.writer.copy(&mut reader, buf);
            while let Some(event) = stream.next().await {
                match event {
                    CopyEvent::Progress(written) => {
                        total += written as u64;
                        let now = Instant::now();
                        if now.duration_since(last).as_millis() > self.millis_between as u128 {
                            last = now;
                            for (_, pb) in self.state.values_mut() {
                                pb.set(total);
                            }
                        }
                    }
                    CopyEvent::Failure(entity, why) => {
                        let (device, mut pb) = self.state.remove(&entity).expect("missing entity");
                        pb.message(&device, "E", &format!("{}", why));
                        pb.finish();
                    }
                    CopyEvent::SourceFailure(why) => {
                        for (device, pb) in self.state.values_mut() {
                            pb.message(device, "E", &format!("{}", why));
                            pb.finish();
                        }
                        return Err(why).context("error reading from source");
                    }
                    CopyEvent::NoWriters => return Err(anyhow!("no writers left")),
                }
            }
        }

        self.ensure_source_length(total).await?;
        for (_, pb) in self.state.values_mut() {
            pb.set(total);
        }
        Ok(())
    }

    async fn ensure_source_length(&mut self, total: u64) -> anyhow::Result<()> {
        let expected = self.expected_size;
        if total != expected {
            return Err(anyhow!(
                "expanded image size changed: expected {} bytes, read {}",
                expected,
                total
            ));
        }
        let mut extra = [0u8; 1];
        match self.image.read(&mut extra).await {
            Ok(0) => Ok(()),
            Ok(_) => Err(anyhow!("expanded image contains more than {} bytes", expected)),
            Err(why) => Err(why).context("error finishing source stream"),
        }
    }

    async fn sync(&mut self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for (entity, file) in self.writer.iter() {
            if let Err(why) = sync_device_file(file).await {
                failures.push((entity, why));
            }
        }
        for (entity, why) in failures {
            self.writer.remove(entity);
            let (device, mut pb) = self.state.remove(&entity).expect("missing entity");
            pb.message(&device, "E", &format!("unable to synchronize device: {}", why));
            pb.finish();
        }
        if self.writer.is_empty() {
            Err(anyhow!("no writers left"))
        } else {
            Ok(())
        }
    }

    async fn seek(&mut self) -> anyhow::Result<()> {
        for (path, pb) in self.state.values_mut() {
            pb.set(0);
            pb.message(path, "S", "");
        }

        self.image.rewind().await?;

        let mut stream = self.writer.seek(SeekFrom::Start(0));
        while let Some((entity, why)) = stream.next().await {
            let (path, mut pb) = self.state.remove(&entity).expect("missing entity");
            pb.message(&path, "E", &format!("errored seeking to start: {}", why));
            pb.finish();
        }

        Ok(())
    }

    async fn validate(&mut self, buf: &mut [u8]) -> anyhow::Result<()> {
        for (path, pb) in self.state.values_mut() {
            pb.set(0);
            pb.message(path, "V", "");
        }

        let copy_bufs = &mut Vec::new();
        let mut total = 0;
        let mut stream = self.writer.validate(&mut self.image, buf, copy_bufs);

        while let Some(event) = stream.next().await {
            match event {
                ValidationEvent::Progress(written) => {
                    total += written as u64;
                    for (_, pb) in self.state.values_mut() {
                        pb.set(total);
                    }
                }
                ValidationEvent::Failure(entity, why) => {
                    let (path, mut pb) = self.state.remove(&entity).expect("missing entity");
                    pb.message(&path, "E", &format!("{}", why));
                    pb.finish();
                }
                ValidationEvent::SourceFailure(why) => {
                    for (path, pb) in self.state.values_mut() {
                        pb.message(path, "E", &format!("error reading from source: {}", why));
                        pb.finish();
                    }

                    return Err(why).context("error reading from source");
                }
                ValidationEvent::NoWriters => return Err(anyhow!("no writers left")),
            }
        }

        Ok(())
    }
}
