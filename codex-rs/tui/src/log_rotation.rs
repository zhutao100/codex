use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

const TUI_LOG_FILE_NAME: &str = "codex-tui.log";
const MAX_TUI_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_ROTATED_TUI_LOG_FILES: usize = 5;

pub(crate) fn open_tui_log(log_dir: &Path) -> io::Result<RotatingLogFile> {
    RotatingLogFile::open(
        log_dir.join(TUI_LOG_FILE_NAME),
        MAX_TUI_LOG_BYTES,
        MAX_ROTATED_TUI_LOG_FILES,
    )
}

pub(crate) struct RotatingLogFile {
    path: PathBuf,
    file: Option<File>,
    max_bytes: u64,
    max_rotated_files: usize,
    current_bytes: u64,
}

impl RotatingLogFile {
    fn open(path: PathBuf, max_bytes: u64, max_rotated_files: usize) -> io::Result<Self> {
        rotate_existing_if_needed(&path, max_bytes, max_rotated_files)?;
        let file = open_append_file(&path)?;
        let current_bytes = file.metadata()?.len();

        Ok(Self {
            path,
            file: Some(file),
            max_bytes,
            max_rotated_files,
            current_bytes,
        })
    }

    fn file_mut(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is closed"))
    }

    fn rotate_if_needed(&mut self, incoming_bytes: usize) -> io::Result<()> {
        if self.current_bytes == 0
            || self.current_bytes.saturating_add(incoming_bytes as u64) <= self.max_bytes
        {
            return Ok(());
        }

        self.rotate()
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        rotate_log_files(&self.path, self.max_rotated_files)?;
        self.file = Some(open_append_file(&self.path)?);
        self.current_bytes = 0;
        Ok(())
    }
}

impl Write for RotatingLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(buf.len())?;
        let written = self.file_mut()?.write(buf)?;
        self.current_bytes = self.current_bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.rotate_if_needed(buf.len())?;
        self.file_mut()?.write_all(buf)?;
        self.current_bytes = self.current_bytes.saturating_add(buf.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file_mut()?.flush()
    }
}

fn open_append_file(path: &Path) -> io::Result<File> {
    let mut log_file_opts = OpenOptions::new();
    log_file_opts.create(true).append(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_file_opts.mode(0o600);
    }

    log_file_opts.open(path)
}

fn rotate_existing_if_needed(
    path: &Path,
    max_bytes: u64,
    max_rotated_files: usize,
) -> io::Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    if metadata.len() < max_bytes {
        return Ok(());
    }

    rotate_log_files(path, max_rotated_files)
}

fn rotate_log_files(path: &Path, max_rotated_files: usize) -> io::Result<()> {
    if max_rotated_files == 0 {
        return remove_file_if_exists(path);
    }

    for index in (1..=max_rotated_files).rev() {
        let rotated = rotated_log_path(path, index);
        if index == max_rotated_files {
            remove_file_if_exists(&rotated)?;
        } else {
            rename_if_exists(&rotated, &rotated_log_path(path, index + 1))?;
        }
    }

    rename_if_exists(path, &rotated_log_path(path, 1))
}

fn rotated_log_path(path: &Path, index: usize) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsStr::to_os_string)
        .unwrap_or_else(|| TUI_LOG_FILE_NAME.into());
    file_name.push(format!(".{index}"));
    path.with_file_name(file_name)
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn rotates_existing_log_when_opened() -> io::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let log_path = temp_dir.path().join(TUI_LOG_FILE_NAME);
        std::fs::write(&log_path, b"old")?;

        let mut log = RotatingLogFile::open(log_path.clone(), 2, 2)?;
        log.write_all(b"new")?;
        log.flush()?;

        assert_eq!(std::fs::read(&log_path)?, b"new");
        assert_eq!(std::fs::read(rotated_log_path(&log_path, 1))?, b"old");
        assert!(!rotated_log_path(&log_path, 2).exists());
        Ok(())
    }

    #[test]
    fn rotates_while_writing_and_retains_archive_limit() -> io::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let log_path = temp_dir.path().join(TUI_LOG_FILE_NAME);
        let mut log = RotatingLogFile::open(log_path.clone(), 4, 2)?;

        log.write_all(b"abcd")?;
        log.write_all(b"ef")?;
        log.write_all(b"ghij")?;
        log.write_all(b"kl")?;
        log.flush()?;

        assert_eq!(std::fs::read(&log_path)?, b"kl");
        assert_eq!(std::fs::read(rotated_log_path(&log_path, 1))?, b"ghij");
        assert_eq!(std::fs::read(rotated_log_path(&log_path, 2))?, b"ef");
        assert!(!rotated_log_path(&log_path, 3).exists());
        Ok(())
    }
}
