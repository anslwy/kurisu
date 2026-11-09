//! One active consumer per bot across homes in the user's shared data directory.
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

pub struct BotLease {
    _file: File,
}

pub fn registry() -> Result<PathBuf> {
    let base = if let Some(base) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|path|path.is_absolute()) {
        base
    } else {
        let home = std::env::var_os("HOME")
            .context("HOME is required for the shared Discord bot registry")?;
        PathBuf::from(home).join(".local/share")
    };
    Ok(base.join("kurisu/discord-bots"))
}
impl BotLease {
    pub fn acquire(root: &Path, bot: &str, home: &Path) -> Result<Self> {
        if bot.is_empty()
            || !bot
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!("Invalid Discord bot ID");
        }
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let meta = std::fs::symlink_metadata(root)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            bail!("Discord bot registry must be a private directory owned by this user");
        }
        let path = root.join(format!("{bot}.lock"));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            bail!("Discord bot lease must be a private file owned by this user");
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            let mut owner = String::new();
            let _ = std::io::Read::by_ref(&mut file)
                .take(2048)
                .read_to_string(&mut owner);
            let owner = serde_json::from_str::<serde_json::Value>(&owner)
                .ok()
                .and_then(|v| v["home"].as_str().map(str::to_string))
                .unwrap_or_else(|| "another home".into());
            bail!("Discord bot {bot} is already active in {owner}; stop that home's Discord channel or use a different bot");
        }
        // Never unlink the lease: replacing its inode would permit two live owners.
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(
            serde_json::to_string(&json!({"home":home,"pid":std::process::id()}))?.as_bytes(),
        )?;
        file.sync_data()?;
        Ok(Self { _file: file })
    }
}
