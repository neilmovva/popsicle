use crate::app::events::FlashResult;
use anyhow::{ensure, Context};
use atomic::Atomic;
use futures::executor;
use popsicle::{DiskDevice, ImageReader, Progress, Task};
use std::cell::Cell;
use std::fmt::{self, Debug, Display, Formatter};
use std::fs::File;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq)]
pub enum FlashStatus {
    Inactive,
    Active,
    Killing,
}

unsafe impl bytemuck::NoUninit for FlashStatus {}

pub struct FlashRequest {
    source: Option<File>,
    expanded_size: u64,
    destinations: Vec<Arc<DiskDevice>>,
    status: Arc<Atomic<FlashStatus>>,
    progress: Arc<Vec<Atomic<u64>>>,
    finished: Arc<Vec<Atomic<bool>>>,
}

pub struct FlashTask {
    pub progress: Arc<Vec<Atomic<u64>>>,
    pub previous: Arc<Mutex<Vec<[u64; 7]>>>,
    pub finished: Arc<Vec<Atomic<bool>>>,
}

struct FlashProgress<'a> {
    request: &'a FlashRequest,
    id: usize,
    errors: &'a [Cell<Result<(), FlashError>>],
}

#[derive(Clone, Debug)]
pub struct FlashError {
    kind: String,
    message: String,
}

impl Display for FlashError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for FlashError {}

impl<'a> Progress for FlashProgress<'a> {
    type Device = ();

    fn message(&mut self, _device: &(), kind: &str, message: &str) {
        self.errors[self.id]
            .set(Err(FlashError { kind: kind.to_string(), message: message.to_string() }));
    }

    fn finish(&mut self) {
        self.request.finished[self.id].store(true, Ordering::SeqCst);
    }

    fn set(&mut self, value: u64) {
        self.request.progress[self.id].store(value, Ordering::SeqCst);
    }
}

impl FlashRequest {
    pub fn new(
        source: File,
        expanded_size: u64,
        destinations: Vec<Arc<DiskDevice>>,
        status: Arc<Atomic<FlashStatus>>,
        progress: Arc<Vec<Atomic<u64>>>,
        finished: Arc<Vec<Atomic<bool>>>,
    ) -> FlashRequest {
        FlashRequest {
            source: Some(source),
            expanded_size,
            destinations,
            status,
            progress,
            finished,
        }
    }

    pub fn write(mut self) -> FlashResult {
        self.status.store(FlashStatus::Active, Ordering::SeqCst);

        let source = self.source.take().unwrap();
        let res = self.write_inner(source);

        for atomic in self.finished.iter() {
            atomic.store(true, Ordering::SeqCst);
        }

        self.status.store(FlashStatus::Inactive, Ordering::SeqCst);

        res
    }

    fn write_inner(&self, source: File) -> FlashResult {
        // Fully inspect the stream again before macOS requests authorization or
        // any destination is unmounted.
        let mut image =
            ImageReader::new(source).context("could not open the selected image for flashing")?;
        let expanded_size = executor::block_on(image.inspect())
            .context("could not decode the selected image before flashing")?;
        ensure!(
            self.expanded_size == expanded_size,
            "selected image decoded size changed after inspection: expected {} bytes, found {}; choose the image again",
            self.expanded_size,
            expanded_size
        );

        let devices = self.destinations.iter().map(|device| (**device).clone());
        let files = popsicle::prepare_devices_for_image(devices, expanded_size)?;

        let mut errors = vec![Ok(()); files.len()];
        let errors_cells = Cell::from_mut(&mut errors as &mut [_]).as_slice_of_cells();

        // How many bytes to write at a given time.
        let mut bucket = [0u8; 64 * 1024];

        let mut task = Task::new(image, false, expanded_size);
        for (i, file) in files.into_iter().enumerate() {
            let progress = FlashProgress { request: self, errors: errors_cells, id: i };
            task.subscribe(file, (), progress);
        }

        let res = executor::block_on(task.process(&mut bucket));

        Ok((res, errors))
    }
}
