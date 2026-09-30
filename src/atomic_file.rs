use std::io::Write;
use std::path::Path;

pub(crate) fn write(destination: &Path, contents: &[u8]) -> std::io::Result<()> {
    let permissions = std::fs::metadata(destination).ok().map(|meta| meta.permissions());
    let temporary = destination.with_extension(format!(
        "{}.{}.tmp",
        destination
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("file"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        replace(&temporary, destination)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(source, destination)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(
                existing_file_name: *const u16,
                new_file_name: *const u16,
                flags: u32,
            ) -> i32;
        }

        let source = source
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let destination = destination
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let replaced = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.tmp");
        let destination = directory.path().join("destination.json");
        std::fs::write(&source, "new").unwrap();
        std::fs::write(&destination, "old").unwrap();

        replace(&source, &destination).unwrap();

        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "new");
        assert!(!source.exists());
    }

    #[test]
    fn writes_an_existing_file_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("runtime.yaml");
        std::fs::write(&destination, "old").unwrap();

        write(&destination, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "new");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("runtime.yaml");
        std::fs::write(&destination, "old").unwrap();
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).unwrap();

        write(&destination, b"new").unwrap();

        let mode = std::fs::metadata(&destination).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
