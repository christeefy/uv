use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use futures::StreamExt;
use rustc_hash::FxHashSet;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::warn;

use uv_distribution_filename::SourceDistExtension;

use crate::Error;

const DEFAULT_BUF_SIZE: usize = 128 * 1024;

/// Unpack a `.zip` archive into the target directory, without requiring `Seek`.
///
/// This is useful for unzipping files as they're being downloaded. If the archive
/// is already fully on disk, consider using `unzip_archive`, which can use multiple
/// threads to work faster in that case.
pub async fn unzip<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    target: impl AsRef<Path>,
) -> Result<(), Error> {
    /// Ensure the file path is safe to use as a [`Path`].
    ///
    /// See: <https://docs.rs/zip/latest/zip/read/struct.ZipFile.html#method.enclosed_name>
    pub(crate) fn enclosed_name(file_name: &str) -> Option<PathBuf> {
        if file_name.contains('\0') {
            return None;
        }
        let path = PathBuf::from(file_name);
        let mut depth = 0usize;
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => return None,
                Component::ParentDir => depth = depth.checked_sub(1)?,
                Component::Normal(_) => depth += 1,
                Component::CurDir => (),
            }
        }
        Some(path)
    }

    let target = target.as_ref();
    let mut reader = futures::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, reader.compat());
    let mut zip = async_zip::base::read::stream::ZipFileReader::new(&mut reader);

    let mut directories = FxHashSet::default();

    while let Some(mut entry) = zip.next_with_entry().await? {
        // Construct the (expected) path to the file on-disk.
        let path = entry.reader().entry().filename().as_str()?;

        // Sanitize the file name to prevent directory traversal attacks.
        let Some(relpath) = enclosed_name(path) else {
            warn!("Skipping unsafe file name: {path}");

            // Close current file prior to proceeding, as per:
            // https://docs.rs/async_zip/0.0.16/async_zip/base/read/stream/
            zip = entry.skip().await?;
            continue;
        };
        let path = target.join(&relpath);
        let is_dir = entry.reader().entry().dir()?;

        // Either create the directory or write the file to disk.
        if is_dir {
            if directories.insert(path.clone()) {
                fs_err::tokio::create_dir_all(path).await?;
            }
        } else {
            if let Some(parent) = path.parent() {
                if directories.insert(parent.to_path_buf()) {
                    fs_err::tokio::create_dir_all(parent).await?;
                }
            }

            // We don't know the file permissions here, because we haven't seen the central directory yet.
            let file = fs_err::tokio::File::create(&path).await?;
            let size = entry.reader().entry().uncompressed_size();
            let mut writer = if let Ok(size) = usize::try_from(size) {
                tokio::io::BufWriter::with_capacity(std::cmp::min(size, 1024 * 1024), file)
            } else {
                tokio::io::BufWriter::new(file)
            };
            let mut reader = entry.reader_mut().compat();
            tokio::io::copy(&mut reader, &mut writer).await?;

            // Validate the CRC of any file we unpack
            // (It would be nice if async_zip made it harder to Not do this...)
            let reader = reader.into_inner();
            let computed = reader.compute_hash();
            let expected = reader.entry().crc32();
            if computed != expected {
                let error = Error::BadCrc32 {
                    path: relpath,
                    computed,
                    expected,
                };
                // There are some cases where we fail to get a proper CRC.
                // This is probably connected to out-of-line data descriptors
                // which are problematic to access in a streaming context.
                // In those cases the CRC seems to reliably be stubbed inline as 0,
                // so we downgrade this to a (hidden-by-default) warning.
                if expected == 0 {
                    warn!("presumed missing CRC: {error}");
                } else {
                    return Err(error);
                }
            }
        }

        // Close current file prior to proceeding, as per:
        // https://docs.rs/async_zip/0.0.16/async_zip/base/read/stream/
        zip = entry.skip().await?;
    }

    // On Unix, we need to set file permissions, which are stored in the central directory, at the
    // end of the archive. The `ZipFileReader` reads until it sees a central directory signature,
    // which indicates the first entry in the central directory. So we continue reading from there.
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let mut directory = async_zip::base::read::cd::CentralDirectoryReader::new(&mut reader);
        while let Some(entry) = directory.next().await? {
            if entry.dir()? {
                continue;
            }

            let Some(mode) = entry.unix_permissions() else {
                continue;
            };

            // The executable bit is the only permission we preserve, otherwise we use the OS defaults.
            // https://github.com/pypa/pip/blob/3898741e29b7279e7bffe044ecfbe20f6a438b1e/src/pip/_internal/utils/unpacking.py#L88-L100
            let has_any_executable_bit = mode & 0o111;
            if has_any_executable_bit != 0 {
                // Construct the (expected) path to the file on-disk.
                let path = entry.filename().as_str()?;
                let Some(path) = enclosed_name(path) else {
                    continue;
                };
                let path = target.join(path);

                let permissions = fs_err::tokio::metadata(&path).await?.permissions();
                if permissions.mode() & 0o111 != 0o111 {
                    fs_err::tokio::set_permissions(
                        &path,
                        Permissions::from_mode(permissions.mode() | 0o111),
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}

/// Unpack the given tar archive into the destination directory.
///
/// This is equivalent to `archive.unpack_in(dst)`, but it also preserves the executable bit.
async fn untar_in(
    mut archive: tokio_tar::Archive<&'_ mut (dyn tokio::io::AsyncRead + Unpin)>,
    dst: &Path,
) -> std::io::Result<()> {
    // Like `tokio-tar`, canonicalize the destination prior to unpacking.
    let dst = fs_err::tokio::canonicalize(dst).await?;

    // Memoize filesystem calls to canonicalize paths.
    let mut memo = FxHashSet::default();

    let mut entries = archive.entries()?;
    let mut pinned = Pin::new(&mut entries);
    while let Some(entry) = pinned.next().await {
        // Unpack the file into the destination directory.
        let mut file = entry?;

        // On Windows, skip symlink entries, as they're not supported. pip recursively copies the
        // symlink target instead.
        if cfg!(windows) && file.header().entry_type().is_symlink() {
            warn!(
                "Skipping symlink in tar archive: {}",
                file.path()?.display()
            );
            continue;
        }

        // Unpack the file into the destination directory.
        #[cfg_attr(not(unix), allow(unused_variables))]
        let unpacked_at = file.unpack_in_raw(&dst, &mut memo).await?;

        // Preserve the executable bit.
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;

            let entry_type = file.header().entry_type();
            if entry_type.is_file() || entry_type.is_hard_link() {
                let mode = file.header().mode()?;
                let has_any_executable_bit = mode & 0o111;
                if has_any_executable_bit != 0 {
                    if let Some(path) = unpacked_at.as_deref() {
                        let permissions = fs_err::tokio::metadata(&path).await?.permissions();
                        if permissions.mode() & 0o111 != 0o111 {
                            fs_err::tokio::set_permissions(
                                &path,
                                Permissions::from_mode(permissions.mode() | 0o111),
                            )
                            .await?;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Unpack a `.tar.gz` archive into the target directory, without requiring `Seek`.
///
/// This is useful for unpacking files as they're being downloaded.
pub async fn untar_gz<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    target: impl AsRef<Path>,
) -> Result<(), Error> {
    let reader = tokio::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, reader);
    let mut decompressed_bytes = async_compression::tokio::bufread::GzipDecoder::new(reader);

    let archive = tokio_tar::ArchiveBuilder::new(
        &mut decompressed_bytes as &mut (dyn tokio::io::AsyncRead + Unpin),
    )
    .set_preserve_mtime(false)
    .set_preserve_permissions(false)
    .set_allow_external_symlinks(false)
    .build();
    Ok(untar_in(archive, target.as_ref()).await?)
}

/// Unpack a `.tar.bz2` archive into the target directory, without requiring `Seek`.
///
/// This is useful for unpacking files as they're being downloaded.
pub async fn untar_bz2<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    target: impl AsRef<Path>,
) -> Result<(), Error> {
    let reader = tokio::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, reader);
    let mut decompressed_bytes = async_compression::tokio::bufread::BzDecoder::new(reader);

    let archive = tokio_tar::ArchiveBuilder::new(
        &mut decompressed_bytes as &mut (dyn tokio::io::AsyncRead + Unpin),
    )
    .set_preserve_mtime(false)
    .set_preserve_permissions(false)
    .set_allow_external_symlinks(false)
    .build();
    Ok(untar_in(archive, target.as_ref()).await?)
}

/// Unpack a `.tar.zst` archive into the target directory, without requiring `Seek`.
///
/// This is useful for unpacking files as they're being downloaded.
pub async fn untar_zst<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    target: impl AsRef<Path>,
) -> Result<(), Error> {
    let reader = tokio::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, reader);
    let mut decompressed_bytes = async_compression::tokio::bufread::ZstdDecoder::new(reader);

    let archive = tokio_tar::ArchiveBuilder::new(
        &mut decompressed_bytes as &mut (dyn tokio::io::AsyncRead + Unpin),
    )
    .set_preserve_mtime(false)
    .set_preserve_permissions(false)
    .set_allow_external_symlinks(false)
    .build();
    Ok(untar_in(archive, target.as_ref()).await?)
}

/// Unpack a `.tar.xz` archive into the target directory, without requiring `Seek`.
///
/// This is useful for unpacking files as they're being downloaded.
pub async fn untar_xz<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    target: impl AsRef<Path>,
) -> Result<(), Error> {
    let reader = tokio::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, reader);
    let mut decompressed_bytes = async_compression::tokio::bufread::XzDecoder::new(reader);

    let archive = tokio_tar::ArchiveBuilder::new(
        &mut decompressed_bytes as &mut (dyn tokio::io::AsyncRead + Unpin),
    )
    .set_preserve_mtime(false)
    .set_preserve_permissions(false)
    .set_allow_external_symlinks(false)
    .build();
    untar_in(archive, target.as_ref()).await?;
    Ok(())
}

/// Unpack a `.tar` archive into the target directory, without requiring `Seek`.
///
/// This is useful for unpacking files as they're being downloaded.
pub async fn untar<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    target: impl AsRef<Path>,
) -> Result<(), Error> {
    let mut reader = tokio::io::BufReader::with_capacity(DEFAULT_BUF_SIZE, reader);

    let archive =
        tokio_tar::ArchiveBuilder::new(&mut reader as &mut (dyn tokio::io::AsyncRead + Unpin))
            .set_preserve_mtime(false)
            .set_preserve_permissions(false)
            .set_allow_external_symlinks(false)
            .build();
    untar_in(archive, target.as_ref()).await?;
    Ok(())
}

/// Unpack a `.zip`, `.tar.gz`, `.tar.bz2`, `.tar.zst`, or `.tar.xz` archive into the target directory,
/// without requiring `Seek`.
pub async fn archive<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    ext: SourceDistExtension,
    target: impl AsRef<Path>,
) -> Result<(), Error> {
    match ext {
        SourceDistExtension::Zip => {
            unzip(reader, target).await?;
        }
        SourceDistExtension::Tar => {
            untar(reader, target).await?;
        }
        SourceDistExtension::Tgz | SourceDistExtension::TarGz => {
            untar_gz(reader, target).await?;
        }
        SourceDistExtension::Tbz | SourceDistExtension::TarBz2 => {
            untar_bz2(reader, target).await?;
        }
        SourceDistExtension::Txz
        | SourceDistExtension::TarXz
        | SourceDistExtension::Tlz
        | SourceDistExtension::TarLz
        | SourceDistExtension::TarLzma => {
            untar_xz(reader, target).await?;
        }
        SourceDistExtension::TarZst => {
            untar_zst(reader, target).await?;
        }
    }
    Ok(())
}
