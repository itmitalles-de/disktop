//! English and German terminal UI text. CLI help, errors on stderr and JSON
//! output intentionally stay English and stable.

use crate::{config::Language, helper::HelperError, model::HomeErrorKind, scanner::RootError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    En,
    De,
}

impl Lang {
    /// Choose the text for this language.
    pub fn pick<'a>(self, english: &'a str, german: &'a str) -> &'a str {
        match self {
            Self::En => english,
            Self::De => german,
        }
    }

    /// The POSIX message locale: the first non-empty value of LC_ALL,
    /// LC_MESSAGES and LANG. German locales select German, all others English.
    pub fn from_locale<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Self {
        Self::from_environment(values, None)
    }

    /// The message language as gettext chooses it: the locale from LC_ALL,
    /// LC_MESSAGES or LANG (first non-empty), but the first entry of the
    /// LANGUAGE priority list takes precedence unless that locale is C or
    /// POSIX (or unset), where glibc ignores LANGUAGE.
    pub fn from_environment<'a>(
        values: impl IntoIterator<Item = Option<&'a str>>,
        language: Option<&str>,
    ) -> Self {
        let locale = values.into_iter().flatten().find(|value| !value.is_empty());
        let plain_c = locale
            .is_none_or(|locale| locale == "C" || locale == "POSIX" || locale.starts_with("C."));
        let preferred = (!plain_c)
            .then(|| language?.split(':').find(|entry| !entry.is_empty()))
            .flatten();
        match preferred.or(locale) {
            Some(name) if name.starts_with("de") => Self::De,
            _ => Self::En,
        }
    }

    pub fn system() -> Self {
        let read = |name| std::env::var(name).ok();
        let (all, messages, lang) = (read("LC_ALL"), read("LC_MESSAGES"), read("LANG"));
        let language = read("LANGUAGE");
        Self::from_environment(
            [all.as_deref(), messages.as_deref(), lang.as_deref()],
            language.as_deref(),
        )
    }

    pub fn resolve(setting: Language, system: Self) -> Self {
        match setting {
            Language::Auto => system,
            Language::En => Self::En,
            Language::De => Self::De,
        }
    }

    pub fn settings_problem(self, error: &str) -> String {
        format!("{}: {error}", self.pick("Settings", "Einstellungen"))
    }

    pub fn unknown_keys(self, keys: &[String]) -> String {
        let keys = keys.join(", ");
        match self {
            Self::En => format!("Settings: unknown keys ignored and kept on save: {keys}"),
            Self::De => format!(
                "Einstellungen: unbekannte Schlüssel ignoriert, beim Speichern behalten: {keys}"
            ),
        }
    }

    pub fn device_unavailable(self, name: &str) -> String {
        match self {
            Self::En => format!("Saved device {name} unavailable; using automatic selection"),
            Self::De => format!("Gespeichertes Gerät {name} fehlt; automatische Auswahl aktiv"),
        }
    }

    pub fn saved(self, path: &str) -> String {
        format!("{} {path}", self.pick("Saved", "Gespeichert:"))
    }

    pub fn save_failed(self, error: &str) -> String {
        format!(
            "{}: {error}",
            self.pick("Save failed", "Speichern fehlgeschlagen")
        )
    }

    pub fn root_with_foreign_settings(self) -> String {
        self.pick(
            "Save refused: spctop runs as root with another user's settings; start without sudo",
            "Nicht gespeichert: spctop läuft als root mit fremden Einstellungen; ohne sudo starten",
        )
        .into()
    }

    pub fn sample_failed(self, error: &str) -> String {
        format!(
            "{}: {error}",
            self.pick("Sample failed", "Messung fehlgeschlagen")
        )
    }

    pub fn helper_outdated(self) -> String {
        match self {
            Self::En => "Helper is older than this spctop; fill levels are added without it. \
                         Rerun scripts/setup-helper.sh to update it"
                .into(),
            Self::De => "Helper ist älter als dieses spctop; Füllstände werden ohne ihn ergänzt. \
                         scripts/setup-helper.sh erneut ausführen, um ihn zu aktualisieren"
                .into(),
        }
    }

    /// A home scan problem. `english` is the collector's English message;
    /// its OS error detail is kept as it is.
    pub fn home_error(self, kind: HomeErrorKind, root: &str, english: &str) -> String {
        if self == Self::En {
            return english.to_owned();
        }
        let detail = |prefix: &str| {
            english
                .strip_prefix(prefix)
                .map_or_else(String::new, |rest| {
                    rest.trim_start_matches(':').trim().to_owned()
                })
        };
        let with_detail = |text: &str, detail: String| {
            if detail.is_empty() {
                text.to_owned()
            } else {
                format!("{text}: {detail}")
            }
        };
        match kind {
            HomeErrorKind::Length => "Home-Pfad muss 1 bis 4096 Bytes lang sein".into(),
            HomeErrorKind::Relative => "Home-Pfad muss absolut sein".into(),
            HomeErrorKind::ControlCharacters => {
                "Home-Pfad darf keine Steuerzeichen enthalten".into()
            }
            HomeErrorKind::NotNormalized => {
                "Home-Pfad muss normalisiert sein (keine abschließenden oder doppelten /)".into()
            }
            HomeErrorKind::DotComponents => "Home-Pfad darf keine . oder .. enthalten".into(),
            HomeErrorKind::NotAllowed => format!(
                "{root} liegt unter keinem Helper-Scan-Wurzelpfad in {}",
                crate::scanner::SCAN_ROOTS_PATH
            ),
            HomeErrorKind::StartFailed => {
                with_detail("Scanner kann nicht starten", detail("cannot start scanner"))
            }
            HomeErrorKind::ScanFailed => with_detail(
                &format!("Kann {root} nicht scannen"),
                detail(&format!("cannot scan {root}")),
            ),
        }
    }

    pub fn root_error(self, error: RootError) -> String {
        self.home_error(error.0, "", &error.to_string())
    }

    pub fn helper_error(self, error: HelperError) -> String {
        match self {
            Self::En => error.to_string(),
            Self::De => match error {
                HelperError::NotInstalled => "Helper ist nicht installiert",
                HelperError::TimedOut => "Helper antwortet nicht (Zeitlimit)",
                HelperError::Exited => {
                    "Helper beendet; nach einem Update scripts/setup-helper.sh erneut ausführen"
                }
                HelperError::Interrupted => "Helper unterbrochen",
                HelperError::ResponseTooLarge => "Helper-Antwort überschreitet das Limit",
                HelperError::ProtocolMismatch => {
                    "Helper-Protokoll passt nicht; nach einem Update scripts/setup-helper.sh \
                     erneut ausführen"
                }
            }
            .into(),
        }
    }

    /// An error for the terminal: known kinds translated, others as they are.
    pub fn error_text(self, error: &anyhow::Error) -> String {
        if let Some(helper) = error.downcast_ref::<HelperError>() {
            return self.helper_error(*helper);
        }
        if let Some(root) = error.downcast_ref::<RootError>() {
            return self.root_error(*root);
        }
        format!("{error:#}")
    }

    pub fn not_responding(self) -> &'static str {
        self.pick("not responding", "reagiert nicht")
    }

    pub fn truncated(self, files: &[String]) -> String {
        let files = files.join(", ");
        match self {
            Self::En => format!("{files} exceeded the read limit; later entries are missing"),
            Self::De => format!("{files} über dem Leselimit; spätere Einträge fehlen"),
        }
    }

    pub fn helper_refused(self) -> String {
        match self {
            Self::En => format!(
                "unprivileged: not a helper scan root ({})",
                crate::scanner::SCAN_ROOTS_PATH
            ),
            Self::De => format!(
                "ohne Helper: kein Helper-Scan-Wurzelpfad ({})",
                crate::scanner::SCAN_ROOTS_PATH
            ),
        }
    }

    pub fn helper_failed(self, error: &str) -> String {
        match self {
            Self::En => format!("Helper failed: {error}; using unprivileged collection"),
            Self::De => format!("Helper fehlgeschlagen: {error}; nutze unprivilegierte Erfassung"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_precedence_and_german_detection() {
        assert_eq!(Lang::from_locale([None, None, None]), Lang::En);
        assert_eq!(
            Lang::from_locale([None, None, Some("de_DE.UTF-8")]),
            Lang::De
        );
        assert_eq!(
            Lang::from_locale([Some("C.UTF-8"), None, Some("de_DE.UTF-8")]),
            Lang::En
        );
        assert_eq!(
            Lang::from_locale([Some(""), Some("de_AT.UTF-8"), Some("en_US.UTF-8")]),
            Lang::De
        );
        assert_eq!(Lang::from_locale([None, None, Some("en_GB")]), Lang::En);
        // LANGUAGE's first entry wins, except for the C locale.
        let env = |lang, language| Lang::from_environment([None, None, lang], language);
        assert_eq!(env(Some("en_US.UTF-8"), Some("de_DE:en")), Lang::De);
        assert_eq!(env(Some("de_DE.UTF-8"), Some("en_GB:de")), Lang::En);
        assert_eq!(env(Some("de_DE.UTF-8"), Some(":")), Lang::De);
        assert_eq!(env(Some("C.UTF-8"), Some("de")), Lang::En);
        assert_eq!(env(Some("POSIX"), Some("de")), Lang::En);
        assert_eq!(env(None, Some("de")), Lang::En);
        assert_eq!(
            Lang::from_environment([Some("C"), None, Some("de_DE")], Some("de")),
            Lang::En
        );
        // Backend errors are translated by kind; OS details stay.
        let english = "cannot scan /srv: Permission denied (os error 13)";
        assert_eq!(
            Lang::De.home_error(HomeErrorKind::ScanFailed, "/srv", english),
            "Kann /srv nicht scannen: Permission denied (os error 13)"
        );
        assert_eq!(
            Lang::En.home_error(HomeErrorKind::ScanFailed, "/srv", english),
            english
        );
        let error = crate::scanner::validate_root("home").unwrap_err();
        assert_eq!(Lang::De.root_error(error), "Home-Pfad muss absolut sein");
        let wrapped = anyhow::Error::new(HelperError::TimedOut);
        assert_eq!(
            Lang::De.error_text(&wrapped),
            "Helper antwortet nicht (Zeitlimit)"
        );
        assert_eq!(Lang::En.error_text(&wrapped), "helper timed out");
        assert_eq!(Lang::De.error_text(&anyhow::anyhow!("other")), "other");
        assert_eq!(Lang::resolve(Language::Auto, Lang::De), Lang::De);
        assert_eq!(Lang::resolve(Language::En, Lang::De), Lang::En);
        assert_eq!(Lang::resolve(Language::De, Lang::En), Lang::De);
    }
}
