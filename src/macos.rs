use anyhow::{anyhow, bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use std::{
    ffi::{c_char, c_void, CStr, CString, OsStr},
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    ptr,
    sync::Mutex,
};

const AUTHOPEN: &str = "/usr/libexec/authopen";
const DISKUTIL: &str = "/usr/sbin/diskutil";
const AUTHORIZATION_EXTERNAL_FORM_LENGTH: usize = 32;
const AUTHORIZATION_FLAG_INTERACTION_ALLOWED: u32 = 1 << 0;
const AUTHORIZATION_FLAG_EXTEND_RIGHTS: u32 = 1 << 1;
const AUTHORIZATION_FLAG_DESTROY_RIGHTS: u32 = 1 << 3;
const AUTHORIZATION_FLAG_PREAUTHORIZE: u32 = 1 << 4;
const AUTHORIZATION_FLAG_CANNOT_PREAUTHORIZE: u32 = 1 << 0;
const AUTHORIZATION_ENVIRONMENT_PROMPT: &[u8] = b"prompt\0";
const AUTHORIZATION_ENVIRONMENT_ICON: &[u8] = b"icon\0";

// recvmsg has no MSG_CMSG_CLOEXEC on macOS. Serializing child creation closes
// the brief receive-to-fcntl inheritance window.
static PROCESS_SPAWN_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
pub struct DiskDevice {
    path: PathBuf,
    label: Box<str>,
    size: u64,
    attachment_id: u64,
}

impl DiskDevice {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn label(&self) -> String {
        format!("{} ({})", self.label, self.path.display())
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Identifies this attachment, so a reused disk number cannot select new media.
    pub fn attachment_token(&self) -> String {
        format!("{}:{}", self.path.display(), self.attachment_id)
    }
}

#[derive(Deserialize)]
struct DiskList {
    #[serde(rename = "WholeDisks", default)]
    whole_disks: Vec<String>,
}

#[derive(Deserialize)]
struct DiskInfo {
    #[serde(rename = "BusProtocol")]
    bus_protocol: Option<String>,
    #[serde(rename = "DeviceIdentifier")]
    device_identifier: Option<String>,
    #[serde(rename = "DeviceNode")]
    device_node: Option<String>,
    #[serde(rename = "Internal")]
    internal: Option<bool>,
    #[serde(rename = "MediaName")]
    media_name: Option<String>,
    #[serde(rename = "IORegistryEntryName")]
    registry_name: Option<String>,
    #[serde(rename = "ParentWholeDisk")]
    parent_whole_disk: Option<String>,
    #[serde(rename = "Size")]
    size: Option<u64>,
    #[serde(rename = "TotalSize")]
    total_size: Option<u64>,
    #[serde(rename = "VirtualOrPhysical")]
    virtual_or_physical: Option<String>,
    #[serde(rename = "WholeDisk")]
    whole_disk: Option<bool>,
    #[serde(rename = "Writable")]
    writable: Option<bool>,
    #[serde(rename = "WritableMedia")]
    writable_media: Option<bool>,
}

/// Discover whole, external, physical USB and SD media suitable for flashing.
pub fn discover_devices() -> Result<Vec<DiskDevice>> {
    let output = run_diskutil(&[
        OsStr::new("list"),
        OsStr::new("-plist"),
        OsStr::new("external"),
        OsStr::new("physical"),
    ])?;
    let mut identifiers = parse_plist::<DiskList>(&output.stdout, "disk list")?.whole_disks;
    identifiers.sort();
    identifiers.dedup();

    let mut devices = Vec::new();
    for identifier in identifiers {
        if !is_whole_disk_identifier(&identifier) {
            continue;
        }
        let path = PathBuf::from(format!("/dev/{}", identifier));
        // A disk can disappear between list and info; omit it from this refresh.
        if let Ok(device) = inspect_device(&path) {
            devices.push(device);
        }
    }
    devices.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(devices)
}

/// Revalidate, unmount, authorize, and open every selected destination.
pub fn prepare_devices_for_image<I>(devices: I, image_size: u64) -> Result<Vec<async_std::fs::File>>
where
    I: IntoIterator<Item = DiskDevice>,
{
    let devices = devices.into_iter().collect::<Vec<_>>();
    let mut files = Vec::with_capacity(devices.len());
    for device in &devices {
        if image_size > device.size {
            bail!(
                "decoded image is {} bytes but {} is only {} bytes",
                image_size,
                device.path.display(),
                device.size
            );
        }
        files.push(open_device(device)?.into());
    }

    // Close any remount window left open while later authorization prompts
    // were displayed. No writes have started, so failure safely drops every
    // descriptor acquired above.
    for device in &devices {
        run_diskutil(&[OsStr::new("unmountDisk"), device.path.as_os_str()])?;
        revalidate(device)?;
    }
    Ok(files)
}

fn inspect_device(path: &Path) -> Result<DiskDevice> {
    let output = run_diskutil(&[OsStr::new("info"), OsStr::new("-plist"), path.as_os_str()])?;
    device_from_info(parse_plist(&output.stdout, &path.display().to_string())?, path)
}

fn device_from_info(info: DiskInfo, requested: &Path) -> Result<DiskDevice> {
    let identifier = info
        .device_identifier
        .as_deref()
        .ok_or_else(|| anyhow!("{} has no device identifier", requested.display()))?;
    if !is_whole_disk_identifier(identifier)
        || info.whole_disk != Some(true)
        || info.parent_whole_disk.as_deref() != Some(identifier)
    {
        bail!("{} is not a whole disk", requested.display());
    }
    if info.internal != Some(false) {
        bail!("refusing to flash internal disk {}", requested.display());
    }
    if !matches!(info.virtual_or_physical.as_deref(), Some(value) if value.eq_ignore_ascii_case("physical"))
    {
        bail!("refusing to flash virtual disk {}", requested.display());
    }
    if !matches!(info.bus_protocol.as_deref(), Some(value) if value.eq_ignore_ascii_case("USB") || value.eq_ignore_ascii_case("Secure Digital"))
    {
        bail!("{} is not USB or SD media", requested.display());
    }
    if info.writable != Some(true) || matches!(info.writable_media, Some(false)) {
        bail!("{} is not writable", requested.display());
    }

    let path = PathBuf::from(format!("/dev/{}", identifier));
    if info.device_node.as_deref().map(Path::new) != Some(path.as_path()) {
        bail!("device node for {} does not match its identifier", requested.display());
    }
    let size = info.size.or(info.total_size).unwrap_or(0);
    if size == 0 {
        bail!("{} reports no writable media", requested.display());
    }
    let label = info
        .media_name
        .or(info.registry_name)
        .map(|value| value.trim().trim_end_matches(" Media").to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| identifier.to_owned());

    Ok(DiskDevice {
        attachment_id: registry_entry_id(identifier, requested)?,
        path,
        label: label.into_boxed_str(),
        size,
    })
}

fn open_device(device: &DiskDevice) -> Result<File> {
    revalidate(device)?;
    run_diskutil(&[OsStr::new("unmountDisk"), device.path.as_os_str()])?;
    revalidate(device)?;

    let canonical = fs::canonicalize(&device.path)
        .with_context(|| format!("could not resolve {}", device.path.display()))?;
    if canonical != device.path {
        bail!("{} resolved to an unexpected device node", device.path.display());
    }
    let before = fs::metadata(&canonical)
        .with_context(|| format!("could not inspect {}", canonical.display()))?;
    if !before.file_type().is_block_device() {
        bail!("{} is not a buffered whole-disk device", canonical.display());
    }

    // Prompt for the app-scoped Removable Volumes permission before the
    // privileged helper opens the node.
    trigger_removable_volume_consent(&canonical)?;
    revalidate(device)?;
    let file = open_with_authorization(&canonical)?;

    let after = file
        .metadata()
        .with_context(|| format!("could not inspect authorized {}", canonical.display()))?;
    if !after.file_type().is_block_device() || after.rdev() != before.rdev() {
        bail!("authorized descriptor did not match validated disk {}", canonical.display());
    }
    Ok(file)
}

fn revalidate(device: &DiskDevice) -> Result<()> {
    let current = inspect_device(&device.path)?;
    if current.path != device.path
        || current.size != device.size
        || current.attachment_id != device.attachment_id
    {
        bail!("disk {} changed after it was selected", device.path.display());
    }
    Ok(())
}

fn trigger_removable_volume_consent(path: &Path) -> Result<()> {
    let path = path_cstring(path)?;
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor >= 0 {
        let _ = unsafe { libc::close(descriptor) };
        return Ok(());
    }

    let why = io::Error::last_os_error();
    match why.raw_os_error() {
        Some(libc::EACCES) => Ok(()), // Expected Unix permission failure.
        Some(libc::EPERM) => bail!(
            "Popsicle was denied Removable Volumes access; allow Popsicle in System Settings and try again"
        ),
        _ => Err(why).context("could not request Removable Volumes access"),
    }
}

#[repr(C)]
struct AuthorizationItem {
    name: *const c_char,
    value_length: usize,
    value: *mut c_void,
    flags: u32,
}

#[repr(C)]
struct AuthorizationItemSet {
    count: u32,
    items: *mut AuthorizationItem,
}

#[repr(C)]
struct AuthorizationExternalForm {
    bytes: [u8; AUTHORIZATION_EXTERNAL_FORM_LENGTH],
}

impl AuthorizationExternalForm {
    fn clear(&mut self) {
        for byte in &mut self.bytes {
            unsafe { ptr::write_volatile(byte, 0) };
        }
    }
}

impl Drop for AuthorizationExternalForm {
    fn drop(&mut self) {
        self.clear();
    }
}

struct Authorization(*const c_void);

impl Authorization {
    fn destroy(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { AuthorizationFree(self.0, AUTHORIZATION_FLAG_DESTROY_RIGHTS) };
            self.0 = ptr::null();
        }
    }
}

impl Drop for Authorization {
    fn drop(&mut self) {
        self.destroy();
    }
}

struct AuthorizedRights(*mut AuthorizationItemSet);

impl Drop for AuthorizedRights {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { AuthorizationFreeItemSet(self.0) };
        }
    }
}

fn open_with_authorization(path: &Path) -> Result<File> {
    let (mut authorization, mut external_form) = authorize_open(path)?;
    let (receiver, sender) = UnixStream::pair().context("could not create authopen socket")?;
    set_cloexec(receiver.as_raw_fd())?;
    set_cloexec(sender.as_raw_fd())?;

    let child_stdout = unsafe { Stdio::from_raw_fd(sender.into_raw_fd()) };
    let spawn_guard = PROCESS_SPAWN_LOCK.lock().unwrap_or_else(|lock| lock.into_inner());
    let mut child = Command::new(AUTHOPEN)
        .arg("-extauth")
        .arg("-stdoutpipe")
        .arg("-o")
        .arg((libc::O_RDWR | libc::O_SYNC | libc::O_NOFOLLOW).to_string())
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(child_stdout)
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start authopen")?;

    let stderr_reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        })
    });
    let transfer = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "authopen stdin unavailable"))
        .and_then(|mut stdin| stdin.write_all(&external_form.bytes));
    external_form.clear();

    let file = match transfer {
        Ok(()) => {
            receive_descriptor(&receiver).map(|descriptor| unsafe { File::from_raw_fd(descriptor) })
        }
        Err(why) => Err(why),
    };
    drop(receiver);
    drop(spawn_guard);
    authorization.destroy();

    let status = child.wait().context("could not wait for authopen")?;
    let stderr = match stderr_reader {
        Some(reader) => {
            reader.join().map_err(|_| anyhow!("authopen diagnostic reader panicked"))??
        }
        None => Vec::new(),
    };
    if !status.success() {
        bail!("authopen failed: {}", diagnostics(&stderr));
    }

    file.context("authopen did not return the disk descriptor")
}

fn authorize_open(path: &Path) -> Result<(Authorization, AuthorizationExternalForm)> {
    let mut reference = ptr::null();
    let status = unsafe { AuthorizationCreate(ptr::null(), ptr::null(), 0, &mut reference) };
    authorization_status(status, "create authorization")?;
    if reference.is_null() {
        bail!("Authorization Services returned an empty authorization");
    }
    let authorization = Authorization(reference);

    let mut name = b"sys.openfile.readwrite.".to_vec();
    name.extend_from_slice(path.as_os_str().as_bytes());
    let name = CString::new(name).context("device path contained an embedded NUL")?;
    let mut right = AuthorizationItem {
        name: name.as_ptr(),
        value_length: 0,
        value: ptr::null_mut(),
        flags: 0,
    };
    let rights = AuthorizationItemSet { count: 1, items: &mut right };

    let prompt = CString::new(format!(
        "Popsicle needs administrator authorization to write the removable drive {}.",
        path.display()
    ))?;
    let icon = bundle_icon_path();
    let mut environment_items = [
        AuthorizationItem {
            name: AUTHORIZATION_ENVIRONMENT_PROMPT.as_ptr().cast(),
            value_length: prompt.as_bytes().len(),
            value: prompt.as_ptr().cast_mut().cast(),
            flags: 0,
        },
        AuthorizationItem {
            name: AUTHORIZATION_ENVIRONMENT_ICON.as_ptr().cast(),
            value_length: icon.as_ref().map_or(0, |value| value.as_bytes().len()),
            value: icon.as_ref().map_or(ptr::null_mut(), |value| value.as_ptr().cast_mut().cast()),
            flags: 0,
        },
    ];
    let environment = AuthorizationItemSet {
        count: if icon.is_some() { 2 } else { 1 },
        items: environment_items.as_mut_ptr(),
    };

    let mut returned = ptr::null_mut();
    let status = unsafe {
        AuthorizationCopyRights(
            authorization.0,
            &rights,
            &environment,
            AUTHORIZATION_FLAG_INTERACTION_ALLOWED
                | AUTHORIZATION_FLAG_EXTEND_RIGHTS
                | AUTHORIZATION_FLAG_PREAUTHORIZE,
            &mut returned,
        )
    };
    let returned = AuthorizedRights(returned);
    authorization_status(status, "authorize Popsicle disk access")?;
    if returned.0.is_null() || !is_exact_right(unsafe { &*returned.0 }, &name) {
        bail!(
            "macOS could not preauthorize Popsicle's exact disk right; refusing an unattributed authopen prompt"
        );
    }

    let mut external = AuthorizationExternalForm { bytes: [0; AUTHORIZATION_EXTERNAL_FORM_LENGTH] };
    let status = unsafe { AuthorizationMakeExternalForm(authorization.0, &mut external) };
    authorization_status(status, "externalize authorization")?;
    Ok((authorization, external))
}

fn is_exact_right(rights: &AuthorizationItemSet, expected: &CStr) -> bool {
    if rights.count != 1 || rights.items.is_null() {
        return false;
    }
    let right = unsafe { &*rights.items };
    !right.name.is_null()
        && unsafe { CStr::from_ptr(right.name) }.to_bytes() == expected.to_bytes()
        && right.flags & AUTHORIZATION_FLAG_CANNOT_PREAUTHORIZE == 0
}

fn authorization_status(status: i32, operation: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(anyhow!("{} failed with Authorization Services status {}", operation, status))
    }
}

fn bundle_icon_path() -> Option<CString> {
    let executable = std::env::current_exe().ok()?;
    let contents = executable.parent()?.parent()?;
    let icon = contents.join("Resources/Popsicle.png");
    icon.is_file().then(|| CString::new(icon.as_os_str().as_bytes()).ok()).flatten()
}

fn receive_descriptor(socket: &UnixStream) -> io::Result<RawFd> {
    let mut payload = [0u8; 1];
    let mut control = [0usize; 8];
    let mut iovec = libc::iovec { iov_base: payload.as_mut_ptr().cast(), iov_len: payload.len() };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as libc::socklen_t;

    let received = loop {
        message.msg_controllen = std::mem::size_of_val(&control) as libc::socklen_t;
        message.msg_flags = 0;
        let result = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, 0) };
        if result >= 0 {
            break result;
        }
        let why = io::Error::last_os_error();
        if why.kind() != io::ErrorKind::Interrupted {
            return Err(why);
        }
    };
    if received == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "authopen returned no descriptor",
        ));
    }
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    let expected_length = unsafe { libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) };
    if header.is_null()
        || unsafe { (*header).cmsg_level } != libc::SOL_SOCKET
        || unsafe { (*header).cmsg_type } != libc::SCM_RIGHTS
        || unsafe { (*header).cmsg_len } != expected_length as _
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authopen did not return exactly one file descriptor",
        ));
    }
    let descriptor = unsafe { ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>()) };
    if descriptor < 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid authopen descriptor"));
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || !unsafe { libc::CMSG_NXTHDR(&message, header) }.is_null()
    {
        let _ = unsafe { libc::close(descriptor) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authopen returned a truncated or malformed descriptor message",
        ));
    }
    if let Err(why) = set_cloexec(descriptor) {
        let _ = unsafe { libc::close(descriptor) };
        return Err(why);
    }
    Ok(descriptor)
}

fn set_cloexec(descriptor: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn registry_entry_id(identifier: &str, disk: &Path) -> Result<u64> {
    let identifier = CString::new(identifier)?;
    let matching = unsafe { IOBSDNameMatching(0, 0, identifier.as_ptr()) };
    if matching.is_null() {
        bail!("IOKit could not match {}", disk.display());
    }
    let service = unsafe { IOServiceGetMatchingService(0, matching) };
    if service == 0 {
        bail!("{} disappeared during validation", disk.display());
    }
    let mut entry_id = 0;
    let status = unsafe { IORegistryEntryGetRegistryEntryID(service, &mut entry_id) };
    let _ = unsafe { IOObjectRelease(service) };
    if status != 0 || entry_id == 0 {
        bail!("IOKit could not identify {}", disk.display());
    }
    Ok(entry_id)
}

fn run_diskutil(args: &[&OsStr]) -> Result<Output> {
    let _guard = PROCESS_SPAWN_LOCK.lock().unwrap_or_else(|lock| lock.into_inner());
    let output = Command::new(DISKUTIL).args(args).output().context("could not run diskutil")?;
    if output.status.success() {
        Ok(output)
    } else {
        bail!("diskutil failed: {}", diagnostics(&output.stderr));
    }
}

fn parse_plist<T: DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    plist::from_bytes(bytes).with_context(|| format!("could not parse {}", what))
}

fn diagnostics(bytes: &[u8]) -> String {
    let message = String::from_utf8_lossy(bytes);
    let message = message.trim();
    if message.is_empty() {
        "no diagnostics".to_owned()
    } else {
        message.to_owned()
    }
}

fn path_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("device path contained an embedded NUL")
}

fn is_whole_disk_identifier(identifier: &str) -> bool {
    identifier.strip_prefix("disk").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOBSDNameMatching(main_port: u32, options: u32, bsd_name: *const c_char) -> *const c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *const c_void) -> u32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, entry_id: *mut u64) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
}

#[link(name = "Security", kind = "framework")]
extern "C" {
    fn AuthorizationCreate(
        rights: *const AuthorizationItemSet,
        environment: *const AuthorizationItemSet,
        flags: u32,
        authorization: *mut *const c_void,
    ) -> i32;
    fn AuthorizationCopyRights(
        authorization: *const c_void,
        rights: *const AuthorizationItemSet,
        environment: *const AuthorizationItemSet,
        flags: u32,
        authorized_rights: *mut *mut AuthorizationItemSet,
    ) -> i32;
    fn AuthorizationMakeExternalForm(
        authorization: *const c_void,
        external_form: *mut AuthorizationExternalForm,
    ) -> i32;
    fn AuthorizationFree(authorization: *const c_void, flags: u32) -> i32;
    fn AuthorizationFreeItemSet(items: *mut AuthorizationItemSet) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_whole_disk_identifiers() {
        assert!(is_whole_disk_identifier("disk7"));
        assert!(!is_whole_disk_identifier("disk7s1"));
        assert!(!is_whole_disk_identifier("rdisk7"));
    }
}
