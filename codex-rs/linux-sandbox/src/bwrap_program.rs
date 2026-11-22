use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

/// Never search the command's PATH or working directory for a sandbox launcher.
pub(crate) fn resolve() -> io::Result<PathBuf> {
    let system = Path::new("/usr/bin/bwrap");
    if fs::metadata(system).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
        return Ok(system.into());
    }
    if let Some(digest) = option_env!("CODEX_BWRAP_SHA256") {
        let exe = std::env::current_exe()?;
        let bundled = exe.with_file_name("codex-resources").join("bwrap");
        if bundled.try_exists()? {
            if format!("{:x}", Sha256::digest(fs::read(&bundled)?)) != digest {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bundled bubblewrap digest mismatch",
                ));
            }
            return Ok(bundled);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "bubblewrap is unavailable: install the distribution's /usr/bin/bwrap or reinstall the complete Codex Linux package",
    ))
}
