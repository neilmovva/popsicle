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
