use std::{fs, io, path::{Component, Path, PathBuf}};

pub struct OperationLocks {
    #[cfg(windows)]
    release: Option<std::sync::mpsc::Sender<()>>,
}

#[cfg(windows)]
pub fn acquire_operation_locks(
    roots: &[PathBuf],
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<OperationLocks> {
    let mut names = roots.iter().map(|root| operation_mutex_name(root)).collect::<io::Result<Vec<_>>>()?;
    names.sort();
    names.dedup();

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<io::Result<()>>(1);
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("bsmanager-library-lock".to_owned())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            use windows_sys::Win32::{
                Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT},
                System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
            };

            let mut handles = Vec::with_capacity(names.len());
            let acquisition = (|| -> io::Result<()> {
                for name in names {
                    let wide_name = name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
                    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr()) };
                    if handle.is_null() {
                        return Err(io::Error::last_os_error());
                    }

                    loop {
                        if cancelled.load(Ordering::Relaxed) {
                            unsafe { CloseHandle(handle); }
                            return Err(io::Error::new(io::ErrorKind::Interrupted, "Library operation was cancelled while waiting for another instance"));
                        }

                        let result = unsafe { WaitForSingleObject(handle, 200) };
                        if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
                            handles.push(handle);
                            break;
                        }
                        if result == WAIT_TIMEOUT {
                            continue;
                        }

                        let error = io::Error::last_os_error();
                        unsafe { CloseHandle(handle); }
                        return Err(error);
                    }
                }
                if cancelled.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "Library operation was cancelled while waiting for another instance"));
                }
                Ok(())
            })();

            match acquisition {
                Ok(()) => {
                    if ready_tx.send(Ok(())).is_ok() {
                        let _ = release_rx.recv();
                    }
                }
                Err(error) => { let _ = ready_tx.send(Err(error)); }
            }

            for handle in handles.into_iter().rev() {
                unsafe {
                    ReleaseMutex(handle);
                    CloseHandle(handle);
                }
            }
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(OperationLocks { release: Some(release_tx) }),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(io::Error::new(io::ErrorKind::Other, error.to_string())),
    }
}

#[cfg(not(windows))]
pub fn acquire_operation_locks(
    _roots: &[PathBuf],
    _cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<OperationLocks> {
    Ok(OperationLocks {})
}

impl Drop for OperationLocks {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[cfg(windows)]
fn operation_mutex_name(root: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};

    let identity = normalize_lock_path(root)?.to_string_lossy().to_lowercase();
    let digest = Sha256::digest(identity.as_bytes());
    let hash = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    Ok(format!("Local\\BSManagerLibrary-{hash}"))
}

#[cfg(windows)]
fn normalize_lock_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    if let Ok(path) = fs::canonicalize(&absolute) {
        return Ok(path);
    }

    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Library path cannot be normalized"))?;
        missing.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Library path has no existing parent"))?;
    }

    let mut normalized = fs::canonicalize(existing)?;
    for component in missing.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

#[derive(Debug)]
pub enum LibraryError {
    CopyToSubpath,
    Io(io::Error),
}

impl From<io::Error> for LibraryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn canonicalize_existing_directory(path: &Path) -> Result<PathBuf, String> {
    let resolved = fs::canonicalize(path).map_err(|error| error.to_string())?;
    if !resolved.is_dir() {
        return Err("The selected destination is not a folder".to_owned());
    }
    Ok(resolved)
}

#[cfg(windows)]
pub fn copy_library(source: &Path, destination: &Path) -> Result<bool, LibraryError> {
    use windows_library::{copy_directory, normalized_path, path_is_within};

    fs::create_dir_all(source)?;
    let source = fs::canonicalize(source)?;
    let destination = normalized_path(destination)?;
    if same_path(&source, &destination) {
        return Ok(false);
    }
    if path_is_within(&source, &destination) || path_is_within(&destination, &source) {
        return Err(LibraryError::CopyToSubpath);
    }

    copy_directory(&source, &destination)?;
    Ok(true)
}

#[cfg(not(windows))]
pub fn copy_library(_source: &Path, _destination: &Path) -> Result<bool, LibraryError> {
    Err(LibraryError::Io(io::Error::new(io::ErrorKind::Unsupported, "Library relocation is available only on Windows")))
}

#[cfg(windows)]
pub fn remove_library(path: &Path) -> io::Result<()> {
    windows_library::remove_entry(path)
}

#[cfg(not(windows))]
pub fn remove_library(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Library relocation is available only on Windows"))
}

#[cfg(windows)]
fn same_path(first: &Path, second: &Path) -> bool {
    let first = first.components().collect::<Vec<_>>();
    let second = second.components().collect::<Vec<_>>();
    first.len() == second.len() && first.iter().zip(second.iter()).all(|(left, right)| {
        left.as_os_str().to_string_lossy().eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    })
}

#[cfg(windows)]
mod windows_library {
    use super::*;

    pub fn normalized_path(path: &Path) -> io::Result<PathBuf> {
        match fs::canonicalize(path) {
            Ok(path) => Ok(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Destination parent is missing"))?;
                let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Destination folder name is missing"))?;
                Ok(fs::canonicalize(parent)?.join(name))
            }
            Err(error) => Err(error),
        }
    }

    pub fn path_is_within(parent: &Path, child: &Path) -> bool {
        let parent = parent.components().collect::<Vec<_>>();
        let child = child.components().collect::<Vec<_>>();
        parent.len() < child.len() && parent.iter().zip(child.iter()).all(|(left, right)| {
            left.as_os_str().to_string_lossy().eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
        })
    }

    pub fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
        if destination.exists() && !fs::symlink_metadata(destination)?.is_dir() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Destination library path is not a directory"));
        }
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let source_item = entry.path();
            let destination_item = destination.join(entry.file_name());
            copy_entry(source, destination, &source_item, &destination_item)?;
        }
        Ok(())
    }

    fn copy_entry(source_root: &Path, destination_root: &Path, source: &Path, destination: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(source)?;
        if junction::exists(source)? {
            let target = junction::get_target(source)?;
            return copy_junction(source_root, destination_root, &target, destination);
        }

        if metadata.file_type().is_symlink() {
            let target = fs::read_link(source)?;
            let target = if target.is_absolute() { target } else { source.parent().unwrap_or(source_root).join(target) };
            if fs::metadata(source).is_ok_and(|target_metadata| target_metadata.is_dir()) {
                return copy_junction(source_root, destination_root, &target, destination);
            }
            remove_destination_link(destination)?;
            if destination.exists() && fs::symlink_metadata(destination)?.is_dir() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "A file link conflicts with a destination folder"));
            }
            fs::copy(source, destination)?;
            return Ok(());
        }

        remove_destination_link(destination)?;
        if metadata.is_dir() {
            if destination.exists() && !fs::symlink_metadata(destination)?.is_dir() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "A source folder conflicts with a destination file"));
            }
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy_entry(source_root, destination_root, &entry.path(), &destination.join(entry.file_name()))?;
            }
        } else {
            if destination.exists() && fs::symlink_metadata(destination)?.is_dir() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "A source file conflicts with a destination folder"));
            }
            fs::copy(source, destination)?;
        }
        Ok(())
    }

    fn copy_junction(source_root: &Path, destination_root: &Path, target: &Path, destination: &Path) -> io::Result<()> {
        remove_destination_link(destination)?;
        if destination.exists() {
            let metadata = fs::symlink_metadata(destination)?;
            if metadata.is_dir() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "A directory link conflicts with a destination folder"));
            }
            fs::remove_file(destination)?;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let relocated_target = relative_suffix(source_root, target)
            .map(|relative| destination_root.join(relative))
            .unwrap_or_else(|| target.to_path_buf());
        junction::create(&relocated_target, destination)
    }

    fn relative_suffix(parent: &Path, child: &Path) -> Option<PathBuf> {
        let parent_components = parent.components().collect::<Vec<_>>();
        let child_components = child.components().collect::<Vec<_>>();
        if parent_components.len() > child_components.len() || !parent_components.iter().zip(child_components.iter()).all(|(left, right)| {
            left.as_os_str().to_string_lossy().eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
        }) {
            return None;
        }
        let mut relative = PathBuf::new();
        for component in child_components.iter().skip(parent_components.len()) {
            if let Component::Normal(name) = component {
                relative.push(name);
            }
        }
        Some(relative)
    }

    fn remove_destination_link(path: &Path) -> io::Result<()> {
        if junction::exists(path)? {
            junction::delete(path)?;
            return Ok(());
        }
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() {
            return remove_link_entry(path);
        }
        Ok(())
    }

    pub fn remove_entry(path: &Path) -> io::Result<()> {
        if junction::exists(path)? {
            return junction::delete(path);
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() {
            return remove_link_entry(path);
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                remove_entry(&entry?.path())?;
            }
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    }

    fn remove_link_entry(path: &Path) -> io::Result<()> {
        match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(directory_error) => fs::remove_file(path).or(Err(directory_error)),
        }
    }
}
