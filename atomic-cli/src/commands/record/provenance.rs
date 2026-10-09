use super::*;

impl Record {
    /// The AI authorship parts from CLI flags and environment variables —
    /// the shared vocabulary [`Provenance::from_authorship_parts`] builds
    /// from, so the local body, the routed body and the handler produce
    /// the identical provenance. Environment variables shadow the flags,
    /// as AI tool integrations set them.
    pub(crate) fn authorship_parts(&self) -> Option<AuthorshipParts> {
        // Check if AI-assisted via flag or environment variable
        let ai_enabled = self.ai_assisted
            || std::env::var("ATOMIC_AI_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
        if !ai_enabled {
            return None;
        }

        // Provider is required: without it there is no meaningful
        // provenance to carry.
        let provider = self
            .ai_provider
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_PROVIDER").ok())?;

        let model = self
            .ai_model
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_MODEL").ok())
            .unwrap_or_else(|| "unknown".to_string());

        let tool = self
            .ai_tool
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_TOOL").ok())
            .or_else(|| Some("cli".to_string()));

        let suggestion_type = self
            .ai_suggestion_type
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_SUGGESTION_TYPE").ok())
            .or_else(|| Some("collaborative".to_string()));

        let input_tokens = self.ai_input_tokens.or_else(|| {
            std::env::var("ATOMIC_AI_INPUT_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        let output_tokens = self.ai_output_tokens.or_else(|| {
            std::env::var("ATOMIC_AI_OUTPUT_TOKENS")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        let request_id = self
            .ai_request_id
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_REQUEST_ID").ok());
        let session_id = self
            .ai_session_id
            .clone()
            .or_else(|| std::env::var("ATOMIC_AI_SESSION_ID").ok());

        Some(AuthorshipParts {
            provider,
            model,
            tool,
            suggestion_type,
            input_tokens,
            output_tokens,
            request_id,
            session_id,
        })
    }

    /// Build AI provenance from CLI flags and environment variables.
    ///
    /// The shared parts (see [`Self::authorship_parts`]), plus the fields
    /// only the local body carries today (cost): the wire carries the
    /// parts; this fills what only a local caller knows.
    pub(super) fn build_provenance(&self) -> Option<Provenance> {
        let mut provenance = Provenance::from_authorship_parts(&self.authorship_parts()?);
        let cost = self.ai_cost_usd.or_else(|| {
            std::env::var("ATOMIC_AI_COST_USD")
                .ok()
                .and_then(|s| s.parse().ok())
        });
        if let Some(cost_usd) = cost {
            provenance.cost = Cost::from_usd(cost_usd);
        }
        Some(provenance)
    }

    /// Build RecordOptions from command-line arguments.
    pub(super) fn build_options(&self) -> CliResult<RecordOptions> {
        let algorithm = self.parse_algorithm()?;

        let mut options = RecordOptions::new()
            .with_all(self.all)
            .with_algorithm(algorithm)
            .with_skip_binary(self.skip_binary)
            .allow_conflict_markers(self.allow_conflict_markers)
            .apply_after_record(!self.dry_run)
            .save_to_store(!self.dry_run);

        // Add specific files if provided
        if !self.files.is_empty() {
            options = options.paths(self.files.clone());
        }

        // Set max size if provided
        if let Some(max_size) = self.max_size {
            options = options.with_max_file_size(max_size);
        }

        // Add AI provenance if enabled
        if let Some(provenance) = self.build_provenance() {
            options = options.add_provenance(provenance);
        }

        Ok(options)
    }
}
