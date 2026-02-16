use super::*;

impl Repository {
    // Remote Configuration

    /// Load remote configuration for this repository.
    ///
    /// # Returns
    ///
    /// The remote configuration, which may be empty if no remotes are configured.
    ///
    /// # Errors
    ///
    /// Returns an error if the config file exists but cannot be parsed.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// let remotes = repo.load_remotes()?;
    ///
    /// if let Some(origin) = remotes.get("origin") {
    ///     println!("Origin URL: {}", origin.url);
    /// }
    /// ```
    pub fn load_remotes(&self) -> Result<RemoteConfig, RepositoryError> {
        RemoteConfig::load(self.config_path()).map_err(|e| RepositoryError::Config(e.to_string()))
    }

    /// Save remote configuration for this repository.
    ///
    /// # Arguments
    ///
    /// * `config` - The remote configuration to save
    ///
    /// # Errors
    ///
    /// Returns an error if the config file cannot be written.
    pub fn save_remotes(&self, config: &RemoteConfig) -> Result<(), RepositoryError> {
        config
            .save(self.config_path())
            .map_err(|e| RepositoryError::Config(e.to_string()))
    }

    /// Get a remote by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to look up
    ///
    /// # Returns
    ///
    /// The remote entry if found, or an error if not found.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// let origin = repo.get_remote("origin")?;
    /// println!("URL: {}", origin.url);
    /// ```
    pub fn get_remote(&self, name: &str) -> Result<RemoteEntry, RepositoryError> {
        let config = self.load_remotes()?;
        config
            .get(name)
            .cloned()
            .ok_or_else(|| RepositoryError::RemoteNotFound {
                name: name.to_string(),
            })
    }

    /// Get the default remote.
    ///
    /// Returns the default remote (explicitly marked, or "origin" if it exists,
    /// or the only configured remote).
    ///
    /// # Returns
    ///
    /// A tuple of (name, entry) for the default remote.
    ///
    /// # Errors
    ///
    /// Returns an error if no remotes are configured.
    pub fn get_default_remote(&self) -> Result<(String, RemoteEntry), RepositoryError> {
        let config = self.load_remotes()?;
        config
            .get_default()
            .map(|(name, entry)| (name.to_string(), entry.clone()))
            .ok_or_else(|| RepositoryError::NoRemotesConfigured)
    }

    /// Add a new remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name for the new remote
    /// * `url` - The URL of the remote repository
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A remote with the same name already exists
    /// - The name is invalid
    /// - The URL is invalid
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// repo.add_remote("origin", "https://api.example.com/repo")?;
    /// ```
    pub fn add_remote(&self, name: &str, url: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .add(name, RemoteEntry::new(url))
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Add a new remote and set it as the default.
    ///
    /// # Arguments
    ///
    /// * `name` - The name for the new remote
    /// * `url` - The URL of the remote repository
    pub fn add_remote_default(&self, name: &str, url: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .add(name, RemoteEntry::new_default(url))
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Remove a remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to remove
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist.
    pub fn remove_remote(&self, name: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .remove(name)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Update the URL of an existing remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to update
    /// * `url` - The new URL
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist or the URL is invalid.
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .set_url(name, url)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Set a remote as the default.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to set as default
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist.
    pub fn set_default_remote(&self, name: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .set_default(name)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Rename a remote.
    ///
    /// # Arguments
    ///
    /// * `old_name` - The current name of the remote
    /// * `new_name` - The new name for the remote
    pub fn rename_remote(&self, old_name: &str, new_name: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .rename(old_name, new_name)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// List all configured remotes.
    ///
    /// # Returns
    ///
    /// A vector of (name, entry) tuples for all configured remotes.
    pub fn list_remotes(&self) -> Result<Vec<(String, RemoteEntry)>, RepositoryError> {
        let config = self.load_remotes()?;
        Ok(config
            .iter()
            .map(|(name, entry)| (name.to_string(), entry.clone()))
            .collect())
    }

    /// Check if a remote exists.
    pub fn has_remote(&self, name: &str) -> Result<bool, RepositoryError> {
        let config = self.load_remotes()?;
        Ok(config.contains(name))
    }
}
