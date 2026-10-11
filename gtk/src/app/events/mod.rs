use crate::flash::{FlashError, FlashRequest};
use crate::hash::hasher;

use anyhow::{bail, Context};
use blake2::Blake2b512;
use crossbeam_channel::{Receiver, Sender};
use dbus_udisks2::{DiskDevice, Disks, UDisks2};
use md5::Md5;
use sha1::Sha1;
use sha2::Sha256;
use sha2::Sha512;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub type FlashResult = anyhow::Result<(anyhow::Result<()>, Vec<Result<(), FlashError>>)>;

pub enum UiEvent {
    SetImageLabel(PathBuf),
    ImageInspected(PathBuf, anyhow::Result<u64>),
    RefreshDevices(Box<[Arc<DiskDevice>]>),
    SetHash(io::Result<String>),
    Flash(JoinHandle<FlashResult>),
    Reset,
}

pub enum BackgroundEvent {
    InspectImage(PathBuf),
    GenerateHash(PathBuf, &'static str),
    Flash(FlashRequest),
    RefreshDevices,
}

pub fn background_thread(events_tx: Sender<UiEvent>, events_rx: Receiver<BackgroundEvent>) {
    thread::spawn(move || {
        let mut hashed: HashMap<(PathBuf, &'static str), String> = HashMap::new();

        let mut device_paths = Vec::new();

        loop {
            match events_rx.recv() {
                Ok(BackgroundEvent::InspectImage(path)) => {
                    let result = inspect_image(&path);
                    let _ = events_tx.send(UiEvent::ImageInspected(path, result));
                }
                Ok(BackgroundEvent::GenerateHash(path, kind)) => {
                    // Check if the cache already contains this hash, and return it.
                    if let Some(result) = hashed.get(&(path.clone(), kind)) {
                        let _ = events_tx.send(UiEvent::SetHash(Ok(result.clone())));
                        continue;
                    }

                    // Hash the file at the given path.
                    let result = match kind {
                        "MD5" => hasher::<Md5>(&path),
                        "SHA256" => hasher::<Sha256>(&path),
                        "SHA1" => hasher::<Sha1>(&path),
                        "SHA512" => hasher::<Sha512>(&path),
                        "BLAKE2b" => hasher::<Blake2b512>(&path),
                        _ => Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "hash kind not supported",
                        )),
                    };

                    // If successful, cache the result.
                    if let Ok(ref result) = result {
                        hashed.insert((path.clone(), kind), result.clone());
                    }

                    // Send this result back to the main thread.
                    let _ = events_tx.send(UiEvent::SetHash(result));
                }
                Ok(BackgroundEvent::RefreshDevices) => {
                    // Fetch the current list of USB devices from popsicle.
                    match refresh_devices() {
                        Ok(devices) => {
                            let new_device_paths: Vec<_> =
                                devices.iter().map(|d| d.drive.path.clone()).collect();
                            if new_device_paths != device_paths {
                                device_paths = new_device_paths;
                                let _ = events_tx.send(UiEvent::RefreshDevices(devices));
                            }
                        }
                        Err(why) => eprintln!("failed to refresh devices: {}", why),
                    }
                }
                Ok(BackgroundEvent::Flash(request)) => {
                    let _ = events_tx.send(UiEvent::Flash(
                        thread::Builder::new()
                            .stack_size(10 * 1024 * 1024)
                            .spawn(|| request.write())
                            .unwrap(),
                    ));
                }
                Err(_) => break,
            }
        }
    });
}

fn inspect_image(path: &Path) -> anyhow::Result<u64> {
    let file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut reader = popsicle::ImageReader::new(file)
        .map_err(|why| anyhow::anyhow!("could not decode {}: {}", path.display(), why))?;
    let format = reader.format();
    let expanded_size = futures::executor::block_on(reader.inspect())
        .map_err(|why| anyhow::anyhow!("could not decode {}: {}", path.display(), why))?;
    if is_zstandard_path(path) && format != popsicle::ImageFormat::Zstandard {
        bail!("{} does not contain a Zstandard frame", path.display());
    }
    Ok(expanded_size)
}

pub fn is_supported_image_path(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
        matches!(extension.to_ascii_lowercase().as_str(), "iso" | "img" | "zst" | "zstd")
    })
}

fn is_zstandard_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "zst" | "zstd"))
}

fn refresh_devices() -> anyhow::Result<Box<[Arc<DiskDevice>]>> {
    let udisks = UDisks2::new()?;
    let devices = Disks::new(&udisks).devices;
    let mut devices = devices
        .into_iter()
        .filter(|d| d.drive.connection_bus == "usb" || d.drive.connection_bus == "sdio")
        .filter(|d| d.parent.size != 0)
        .map(Arc::new)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    devices.sort_by_key(|d| d.drive.id.clone());
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::inspect_image;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn fixture(data: &[u8], extension: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "popsicle-gtk-test-{}-{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            extension
        ));
        fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn rejects_a_zstandard_extension_without_a_zstandard_frame() {
        let path = fixture(b"not a Zstandard image", "zst");
        let error = inspect_image(&path).err().expect("mislabeled image must fail");
        assert!(error.to_string().contains("does not contain a Zstandard frame"));
        fs::remove_file(path).unwrap();
    }
}
