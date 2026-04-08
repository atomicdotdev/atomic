use super::*;

impl Repository {
    /// This exports the working copy state at the current (or specified)
    /// Merkle state to the given destination.
    ///
    /// # Arguments
    ///
    /// * `destination` - Path to the output archive or directory
    /// * `options` - Options controlling archive creation
    ///
    /// # Returns
    ///
    /// An `ArchiveOutcome` with details about the created archive.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Archive to a tarball
    /// let outcome = repo.archive("release.tar.gz", ArchiveOptions::default())?;
    ///
    /// // Archive to a directory
    /// let outcome = repo.archive("./release/", ArchiveOptions::directory())?;
    ///
    /// // Archive with a prefix
    /// let outcome = repo.archive("myproject-1.0.tar.gz",
    ///     ArchiveOptions::default().with_prefix("myproject-1.0/"))?;
    /// ```
    pub fn archive<P: AsRef<Path>>(
        &self,
        destination: P,
        options: ArchiveOptions,
    ) -> Result<ArchiveOutcome, RepositoryError> {
        use std::time::Instant;

        let start = Instant::now();
        let dest_path = destination.as_ref();

        // Get current state
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_view);
        let stack = txn
            .get_view(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: stack_name.to_string(),
            })?;

        let state = options.state.unwrap_or(stack.state);

        // Build manifest from tracked files
        let mut manifest = ArchiveManifest::new();
        let tracked_files =
            list_tracked(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        for file in tracked_files {
            // Apply include/exclude filters
            let path_str = file.path.to_string_lossy();
            if !options.should_include(&path_str) {
                continue;
            }

            // Get file info from working copy
            let full_path = self.root.join(&file.path);
            if full_path.is_file() {
                let metadata = std::fs::metadata(&full_path).map_err(RepositoryError::Io)?;
                let size = metadata.len();

                let path_string = file.path.to_string_lossy().to_string();
                let mut entry = ArchiveEntry::file(&path_string, size);

                // Apply prefix if specified
                if let Some(ref prefix) = options.prefix {
                    entry.path = format!("{}{}", prefix, path_string);
                }

                manifest.add(entry);
            } else if full_path.is_dir() {
                let path_string = file.path.to_string_lossy().to_string();
                let mut entry = ArchiveEntry::directory(&path_string);

                if let Some(ref prefix) = options.prefix {
                    entry.path = format!("{}{}", prefix, path_string);
                }

                manifest.add(entry);
            }
        }

        // Check for empty archive
        if manifest.is_empty() {
            return Err(RepositoryError::Archive("No files to archive".to_string()));
        }

        // Check limits
        if let Some(max_files) = options.max_files {
            if manifest.file_count > max_files {
                return Err(RepositoryError::Archive(format!(
                    "Too many files: {} (max {})",
                    manifest.file_count, max_files
                )));
            }
        }

        if let Some(max_size) = options.max_size {
            if manifest.total_size > max_size {
                return Err(RepositoryError::Archive(format!(
                    "Archive too large: {} bytes (max {})",
                    manifest.total_size, max_size
                )));
            }
        }

        // Create the archive based on format
        let archive_size = match options.format {
            crate::archive::ArchiveFormat::Directory => {
                self.archive_to_directory(dest_path, &manifest, &options)?
            }
            _ => {
                // For now, only directory format is fully implemented
                return Err(RepositoryError::Archive(format!(
                    "Archive format '{}' not yet implemented. Use directory format.",
                    options.format
                )));
            }
        };

        let duration = start.elapsed();

        Ok(
            ArchiveOutcome::new(dest_path.to_path_buf(), options.format, state, manifest)
                .with_archive_size(archive_size)
                .with_duration(duration.as_millis() as u64),
        )
    }

    /// Archive to a directory (internal helper).
    fn archive_to_directory(
        &self,
        dest: &Path,
        manifest: &ArchiveManifest,
        options: &ArchiveOptions,
    ) -> Result<u64, RepositoryError> {
        let mut archive = DirectoryArchive::new(dest).map_err(RepositoryError::Io)?;

        let mut total_size = 0u64;

        // First create directories
        for entry in manifest.directories() {
            archive
                .create_directory(&entry.path, entry.mode, 0)
                .map_err(RepositoryError::Io)?;
        }

        // Then copy files
        for entry in manifest.files() {
            // Determine source path - strip prefix if it was added
            let source_rel_path = if let Some(ref prefix) = options.prefix {
                if entry.path.starts_with(prefix) {
                    entry.path[prefix.len()..].to_string()
                } else {
                    entry.path.clone()
                }
            } else {
                entry.path.clone()
            };
            let source_path = self.root.join(&source_rel_path);

            let mut writer = archive
                .create_file(&entry.path, entry.size, entry.mode, 0)
                .map_err(RepositoryError::Io)?;

            // Copy file contents
            let mut source = std::fs::File::open(&source_path).map_err(RepositoryError::Io)?;
            let copied = std::io::copy(&mut source, &mut writer).map_err(RepositoryError::Io)?;
            total_size += copied;

            archive.close_file(writer).map_err(RepositoryError::Io)?;
        }

        archive.finish().map_err(RepositoryError::Io)?;

        Ok(total_size)
    }
}
