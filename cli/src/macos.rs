use anyhow::{bail, Context, Result};
use clap::{builder::ValueParser, Arg, ArgAction, Command};
use popsicle::{
    discover_devices, prepare_devices_for_image, DiskDevice, ImageFormat, ImageReader, Progress,
    Task,
};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

fn command() -> Command {
    Command::new("popsicle")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Flash USB/SD media with Popsicle-scoped macOS authorization")
        .after_help("Output is newline-delimited JSON (schema_version=1). Run list to get an attachment token. Flash erases one disk, requires --yes, and always verifies. Run as your normal login user; macOS prompts for administrator approval when needed.")
        .subcommand_required(true)
        .subcommand(Command::new("list").about("List eligible external physical USB/SD whole disks"))
        .subcommand(Command::new("inspect").about("Validate an image and report expanded size without disk access").arg(image_arg()))
        .subcommand(Command::new("flash").about("Validate, unmount, authorize, write, synchronize, and verify one disk")
            .arg(image_arg())
            .arg(Arg::new("device").long("device").required(true).help("Exact /dev/diskN from list"))
            .arg(Arg::new("token").long("token").required(true).help("Attachment token from list"))
            .arg(Arg::new("yes").long("yes").required(true).action(ArgAction::SetTrue).help("Confirm erasing this disk"))
            .arg(Arg::new("dry-run").long("dry-run").action(ArgAction::SetTrue).help("Check image, identity, capacity without unmounting or authorizing")))
}

fn image_arg() -> Arg {
    Arg::new("image")
        .required(true)
        .value_parser(ValueParser::path_buf())
        .help("Raw ISO/IMG or Zstandard image (detected by contents)")
}

fn emit(mut event: Value) -> io::Result<()> {
    event["schema_version"] = json!(1);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, &event)?;
    out.write_all(b"\n")?;
    out.flush()
}

pub fn main() -> i32 {
    let args = match command().try_get_matches() {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return 0;
        }
        Err(error) => {
            let _ = emit(json!({"event":"error", "code":"usage", "message":error.to_string()}));
            return 2;
        }
    };
    match futures::executor::block_on(run(args)) {
        Ok(()) => 0,
        Err(error) => {
            let _ = emit(
                json!({"event":"error", "code":"operation_failed", "message":format!("{:#}", error)}),
            );
            1
        }
    }
}

fn device_json(device: &DiskDevice) -> Value {
    json!({"path":device.path(), "label":device.label(), "size_bytes":device.size(), "token":device.attachment_token()})
}

fn select_device(devices: Vec<DiskDevice>, path: &str, token: &str) -> Result<DiskDevice> {
    let device = devices.into_iter().find(|device| device.path() == Path::new(path)).context(
        "target is not an eligible external physical USB/SD whole disk; run popsicle list",
    )?;
    if device.attachment_token() != token {
        bail!("target attachment changed; run popsicle list and select the intended drive again");
    }
    Ok(device)
}

async fn run(args: clap::ArgMatches) -> Result<()> {
    let (name, args) = args.subcommand().expect("subcommand required");
    if name == "list" {
        let devices = discover_devices()?;
        emit(
            json!({"event":"devices", "devices":devices.iter().map(device_json).collect::<Vec<_>>()}),
        )?;
        return Ok(());
    }
    let path = args.get_one::<PathBuf>("image").expect("image required");
    // Capture attachment identity before decompression; the backend checks it again before opening.
    let device = if name == "flash" {
        Some(select_device(
            discover_devices()?,
            args.get_one::<String>("device").unwrap(),
            args.get_one::<String>("token").unwrap(),
        )?)
    } else {
        None
    };
    emit(json!({"event":"inspecting", "image":path}))?;
    let source = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut image = ImageReader::new(source)?;
    let format = match image.format() {
        ImageFormat::Raw => "raw",
        ImageFormat::Zstandard => "zstandard",
    };
    let size = image.inspect().await?;
    emit(json!({"event":"image", "image":path, "format":format, "expanded_size_bytes":size}))?;
    let device = match device {
        Some(device) => device,
        None => return Ok(()),
    };
    if size > device.size() {
        bail!("expanded image ({} bytes) exceeds target capacity ({} bytes)", size, device.size());
    }
    if args.get_flag("dry-run") {
        select_device(
            discover_devices()?,
            args.get_one::<String>("device").unwrap(),
            args.get_one::<String>("token").unwrap(),
        )?;
        emit(
            json!({"event":"ready", "dry_run":true, "device":device_json(&device), "expanded_size_bytes":size, "verify":true}),
        )?;
        return Ok(());
    }
    let disk_path = device.path().to_owned();
    emit(
        json!({"event":"authorizing", "device":device_json(&device), "message":"Approve Popsicle's macOS administrator prompt to erase this drive"}),
    )?;
    let files = prepare_devices_for_image([device], size)?;
    let failed = Arc::new(AtomicBool::new(false));
    let mut task = Task::new(image, true, size);
    let progress = JsonProgress {
        path: disk_path.clone(),
        total: size,
        phase: "write",
        failed: failed.clone(),
    };
    task.subscribe(files.into_iter().next().context("no authorized device")?, (), progress);
    emit(
        json!({"event":"started", "device":disk_path, "expanded_size_bytes":size, "verify":true}),
    )?;
    task.process(&mut vec![0; 1024 * 1024]).await?;
    // Some per-device failures arrive through Progress rather than Task's return value.
    if failed.load(Ordering::Relaxed) {
        bail!("flashing or verification failed; no successful completion");
    }
    emit(json!({"event":"complete", "device":disk_path, "bytes_written":size, "verified":true}))?;
    Ok(())
}

struct JsonProgress {
    path: PathBuf,
    total: u64,
    phase: &'static str,
    failed: Arc<AtomicBool>,
}

impl JsonProgress {
    fn output(&self, event: Value) {
        if emit(event).is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
    }
}

impl Progress for JsonProgress {
    type Device = ();
    fn message(&mut self, _: &(), kind: &str, message: &str) {
        match kind {
            "E" => {
                self.failed.store(true, Ordering::Relaxed);
                self.output(json!({"event":"device_error", "device":self.path, "message":message}));
            }
            "S" => self.phase = "seek",
            "V" => self.phase = "verify",
            _ => (),
        }
    }
    fn finish(&mut self) {} // Only the caller emits successful completion.
    fn set(&mut self, value: u64) {
        self.output(json!({"event":"progress", "device":self.path, "phase":self.phase, "bytes":value, "total_bytes":self.total}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flash_requires_explicit_target_token_and_confirmation() {
        for args in [
            vec!["popsicle", "flash", "x.img"],
            vec!["popsicle", "flash", "x.img", "--device", "/dev/disk7", "--token", "x"],
        ] {
            assert!(command().try_get_matches_from(args).is_err());
        }
        assert!(command()
            .try_get_matches_from([
                "popsicle",
                "flash",
                "x.img",
                "--device",
                "/dev/disk7",
                "--token",
                "x",
                "--yes",
                "--dry-run"
            ])
            .is_ok());
    }
    #[test]
    fn missing_target_is_rejected() {
        assert!(select_device(vec![], "/dev/disk0", "x").is_err());
    }
    #[test]
    fn device_error_is_not_success_even_if_task_returns_ok() {
        let failed = Arc::new(AtomicBool::new(false));
        let mut progress = JsonProgress {
            path: PathBuf::from("test"),
            total: 1,
            phase: "write",
            failed: failed.clone(),
        };
        progress.message(&(), "E", "verification mismatch");
        progress.finish();
        assert!(failed.load(Ordering::Relaxed));
    }
}
