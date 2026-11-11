//! Settings that change while the session runs, applied all at once.
//!
//! [`Session::apply_settings`](crate::Session::apply_settings) checks a
//! whole [`SettingsUpdate`] before touching anything: either every field
//! is applied or none is, and readers of
//! [`Session::settings`](crate::Session::settings) never see half of an
//! update.

/// The live settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct LiveSettings {
    /// Session-wide download limit in bytes per second, shared by every
    /// torrent (on top of each torrent's own limit).
    pub download_rate_limit: Option<u64>,
    /// Session-wide upload limit in bytes per second.
    pub upload_rate_limit: Option<u64>,
    /// Session-wide cap on open peer connections (`Some(0)` holds every
    /// dial until raised).
    pub max_connections: Option<usize>,
    /// Torrents downloading at once; the others wait in the queue
    /// (oldest first). `None` is unlimited.
    pub max_active_downloads: Option<usize>,
    /// Torrents seeding at once; the others wait in the queue.
    pub max_active_seeds: Option<usize>,
}

impl LiveSettings {
    /// Whether the queue limits anything.
    pub(crate) const fn queues(&self) -> bool {
        self.max_active_downloads.is_some() || self.max_active_seeds.is_some()
    }
}

/// A change to some live settings: `None` leaves a field alone,
/// `Some(None)` lifts the limit, `Some(Some(n))` sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SettingsUpdate {
    /// See [`LiveSettings::download_rate_limit`].
    pub download_rate_limit: Option<Option<u64>>,
    /// See [`LiveSettings::upload_rate_limit`].
    pub upload_rate_limit: Option<Option<u64>>,
    /// See [`LiveSettings::max_connections`].
    pub max_connections: Option<Option<usize>>,
    /// See [`LiveSettings::max_active_downloads`].
    pub max_active_downloads: Option<Option<usize>>,
    /// See [`LiveSettings::max_active_seeds`].
    pub max_active_seeds: Option<Option<usize>>,
}

/// Smallest accepted rate limit (bytes per second): below one block per
/// second peers time out.
pub const MIN_RATE_LIMIT: u64 = 16 * 1024;

impl From<LiveSettings> for SettingsUpdate {
    /// An update that sets every field to `settings`.
    fn from(settings: LiveSettings) -> Self {
        Self {
            download_rate_limit: Some(settings.download_rate_limit),
            upload_rate_limit: Some(settings.upload_rate_limit),
            max_connections: Some(settings.max_connections),
            max_active_downloads: Some(settings.max_active_downloads),
            max_active_seeds: Some(settings.max_active_seeds),
        }
    }
}

/// Why a [`SettingsUpdate`] was refused (nothing was applied).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SettingsError {
    /// A field has an invalid value.
    #[error("invalid {field}: {reason}")]
    Invalid {
        /// The field.
        field: &'static str,
        /// What is wrong.
        reason: String,
    },
}

impl SettingsUpdate {
    /// The settings after this update, if every field is valid.
    ///
    /// # Errors
    /// The first invalid field.
    pub fn applied_to(&self, current: LiveSettings) -> Result<LiveSettings, SettingsError> {
        let rate = |field: &'static str, v: Option<u64>| match v {
            Some(r) if r < MIN_RATE_LIMIT => Err(SettingsError::Invalid {
                field,
                reason: format!("{r} B/s is below the minimum of {MIN_RATE_LIMIT} B/s"),
            }),
            v => Ok(v),
        };
        let mut next = current;
        if let Some(v) = self.download_rate_limit {
            next.download_rate_limit = rate("download_rate_limit", v)?;
        }
        if let Some(v) = self.upload_rate_limit {
            next.upload_rate_limit = rate("upload_rate_limit", v)?;
        }
        if let Some(v) = self.max_connections {
            next.max_connections = v;
        }
        if let Some(v) = self.max_active_downloads {
            next.max_active_downloads = v;
        }
        if let Some(v) = self.max_active_seeds {
            next.max_active_seeds = v;
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_apply_only_when_all_fields_are_valid() {
        let base = LiveSettings::default();
        let ok = SettingsUpdate {
            download_rate_limit: Some(Some(1 << 20)),
            max_active_downloads: Some(Some(2)),
            ..SettingsUpdate::default()
        };
        let next = ok.applied_to(base).unwrap();
        assert_eq!(next.download_rate_limit, Some(1 << 20));
        assert_eq!(next.max_active_downloads, Some(2));
        assert_eq!(next.upload_rate_limit, None);
        assert!(next.queues());
        let bad = SettingsUpdate {
            max_connections: Some(Some(3)),
            upload_rate_limit: Some(Some(1)),
            ..SettingsUpdate::default()
        };
        assert!(matches!(
            bad.applied_to(next),
            Err(SettingsError::Invalid {
                field: "upload_rate_limit",
                ..
            })
        ));
        let slow = SettingsUpdate {
            download_rate_limit: Some(Some(10)),
            ..SettingsUpdate::default()
        };
        assert!(slow.applied_to(next).is_err());
        // Lifting a limit.
        let lift = SettingsUpdate {
            download_rate_limit: Some(None),
            max_active_downloads: Some(None),
            ..SettingsUpdate::default()
        };
        assert_eq!(lift.applied_to(next).unwrap(), LiveSettings::default());
    }
}
