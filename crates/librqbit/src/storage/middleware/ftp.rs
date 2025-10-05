use std::io::Cursor;
use async_ftp::FtpStream;
use url::Url;
use async_ftp::FtpStream;
use tokio::io::AsyncReadExt;
use url::Url;

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::storage::{StorageFactory, TorrentStorage};
use crate::torrent_state::TorrentMetadata;
use crate::ManagedTorrentShared;

pub const FTP_PROTOCOLS: &[&str] = &[
    // "sftp://", // ?
    "ftp://",
    "ftps://",
];

pub struct FtpStorageFactory {
    url: String,
}

impl FtpStorageFactory {
    pub fn new(url: String) -> Self {
        Self { url }
    }
}

// impl<'a> StorageFactory for FtpStorageFactory {
impl StorageFactory for FtpStorageFactory {
    // type Storage = FtpStorage<'a>;
    type Storage = Box<dyn TorrentStorage + 'static>;

    fn create(
        &self,
        // shared: &'a ManagedTorrentShared,
        // metadata: &'a TorrentMetadata
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata
    // ) -> anyhow::Result<FtpStorage<'a>> {
    ) -> anyhow::Result<Self::Storage> {
        // log::info!("Creating FtpStorage for URL: {}", self.url);

        // let fs = RemoteFs::from_url(&self.url)
        //     .map_err(|e| anyhow::anyhow!("remotefs error: {e}"))?;

        // https://github.com/veeso/termscp/blob/05830db206605f60ce54e23ca5df7de02115f491/src/filetransfer/remotefs_builder.rs#L109-L13
        let secure = true;
        let parsed = Url::parse(&self.url)?;
        let protocol = parsed.scheme().to_string();
        if (protocol != "ftp") && (protocol != "ftps") {
            anyhow::bail!("FtpStorageFactory only supports ftp:// and ftps:// URLs, got: {protocol}");
        }
        let hostname = parsed.host_str().ok_or_else(|| anyhow::anyhow!("Missing host"))?.to_string();
        let port = parsed.port().unwrap_or(21);
        let username = if parsed.username().is_empty() {
            None
        } else {
            Some(parsed.username().to_string())
        };
        let password = parsed.password().map(|s| s.to_string());
        let path = parsed.path().to_string();
        let mut fs = FtpFs::new(hostname, port).passive_mode();
        if let Some(_username) = username {
            fs = fs.username(_username);
        }
        if let Some(_password) = password {
            fs = fs.password(_password);
        }
        if secure {
            fs = fs.secure(true, true);
        }


impl FtpStorage {
    pub fn new(host: String, port: u16, username: String, password: String) -> Self {
        Self { host, port, username, password }
    }

}


        Ok(Box::new(FtpStorage {
            fs: Arc::new(fs),
            shared,
            metadata,
        }))
    }

    fn clone_box(&self) -> Box<dyn StorageFactory<Storage = Self::Storage> + 'static> {
        Box::new(Self {
            url: self.url.clone(),
        })
    }
}

pub struct FtpStorage<'a> {
    // fs: Arc<dyn RemoteFs>,
    shared: &'a ManagedTorrentShared,
    metadata: &'a TorrentMetadata,
    host: String,
    port: u16,
    username: String,
    password: String,
}

// WONTFIX remotefs is not thread-safe, so we cannot implement Sync or Send for FtpStorage
// the trait `Sync` is not implemented for `(dyn RemoteFs + 'static)`
// https://github.com/remotefs-rs/remotefs-rs/pull/21
impl<'a> TorrentStorage for FtpStorage<'a> {
    async fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let file_details = self.metadata.file_infos[file_id].clone();

        let relative_path = &file_details.relative_filename;

        // let mut full_path = self.output_folder.clone();
        // full_path.push(relative_path);

        if file_details.attrs.padding {
            log::info!("Skipping read for padding file: {:?}", relative_path);
            return Ok(());
        };
        log::info!("FtpStorage pread_exact: file_id={}, offset={}, len={}, path={:?}", file_id, offset, buf.len(), relative_path);

        /*
        // TODO prepend "/" before relative_path?
        // NOTE only some remotefs backends support "open"
        let mut file = self.fs.open(relative_path)
            .map_err(|e| anyhow::anyhow!("remotefs open error: {e}"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| anyhow::anyhow!("remotefs seek error: {e}"))?;
        let mut read_total = 0;
        while read_total < buf.len() {
            let n = file.read(&mut buf[read_total..])
                .map_err(|e| anyhow::anyhow!("remotefs read error: {e}"))?;
            if n == 0 {
                return Err(anyhow::anyhow!(
                    "remotefs: unexpected EOF (read {} of {} bytes)",
                    read_total,
                    buf.len()
                ));
            }
            read_total += n;
        }
        */
        let addr: String = format!("{}:{}", self.host, self.port);
        let mut ftp = FtpStream::connect(addr).await?;
        ftp.login(&self.username, &self.password).await?;
        let mut reader = ftp.retr(relative_path).await?;
        reader.read_exact(&mut vec![0u8; offset as usize]).await?; // skip to offset
        reader.read_exact(buf).await?;
        ftp.quit().await?;
        Ok(())
    }

    fn pwrite_all(&self, _file_id: usize, _offset: u64, _buf: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("FtpStorage is read-only: pwrite_all is not implemented");
    }

    fn remove_file(&self, _file_id: usize, _filename: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("FtpStorage is read-only: remove_file is not implemented");
    }

    fn remove_directory_if_empty(&self, _path: &std::path::Path) -> anyhow::Result<()> {
        anyhow::bail!("FtpStorage is read-only: remove_directory_if_empty is not implemented");
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        anyhow::bail!("FtpStorage is read-only: ensure_file_length is not implemented");
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        anyhow::bail!("FtpStorage take is not implemented")
    }

    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        // No-op for FtpStorage, as it is read-only and uses references.
        // let mut files = Vec::<OpenedFile>::new();
        // for file_details in metadata.file_infos.iter() {
        //     let mut full_path = self.output_folder.clone();
        //     let relative_path = &file_details.relative_filename;
        //     full_path.push(relative_path);

        //     if file_details.attrs.padding {
        //         files.push(OpenedFile::new_dummy());
        //         continue;
        //     };
        //     std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
        //     if shared.options.allow_overwrite {
        //         let (file, writeable) = match
        //         OpenOptions::new()
        //             .create(true)
        //             .truncate(false)
        //             .read(true)
        //             .write(true)
        //             .open(&full_path)
        //         {
        //             Ok(file) => (file, true),
        //             Err(e) => {
        //                 warn!(?full_path, "error opening file in create+write mode: {e:?}");
        //                 // open the file in read-only mode, will reopen in write mode later.
        //                 (
        //                     OpenOptions::new()
        //                         .create(false)
        //                         .read(true)
        //                         .open(&full_path)
        //                         .with_context(|| format!("error opening {full_path:?}"))?,
        //                     false,
        //                 )
        //             }
        //         };
        //         files.push(OpenedFile::new(full_path.clone(), file, writeable));
        //     } else {
        //         // create_new does not seem to work with read(true), so calling this twice.
        //         let file = OpenOptions::new()
        //             .create_new(true)
        //             .write(true)
        //             .open(&full_path)
        //             .with_context(|| {
        //                 format!(
        //                     "error creating a new file (because allow_overwrite = false) {:?}",
        //                     &full_path
        //                 )
        //             })?;
        //         OpenOptions::new().read(true).write(true).open(&full_path)?;
        //         let writeable = true;
        //         files.push(OpenedFile::new(full_path.clone(), file, writeable));
        //     };
        // }

        // self.opened_files = files;
        Ok(())
    }
}



