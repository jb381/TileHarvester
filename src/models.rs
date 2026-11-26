//! Domain values persisted by SQLite or returned by activity workflows.

use std::{fmt, str::FromStr};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use crate::error::Error;

macro_rules! domain_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? } $(aliases { $($alias:literal => $canonical:ident),+ })?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(value: &str) -> Result<Self, Error> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    $($($alias => Ok(Self::$canonical),)+)?
                    _ => Err(Error::Validation(format!("Unknown {}: {value:?}", stringify!($name)))),
                }
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value.as_str()?.parse().map_err(|error| FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

domain_enum!(ActivityStatus {
    Pending => "pending",
    Processed => "processed",
    SkippedNoGps => "skipped_no_gps",
    SkippedIgnoredSport => "skipped_ignored_sport",
    Failed => "failed",
});
domain_enum!(AnnotationStatus {
    None => "none",
    Done => "done",
    Skipped => "skipped",
    Failed => "failed",
} aliases { "updated" => Done });
domain_enum!(TileKind { Squadrat => "squadrat", Squadratinho => "squadratinho" });
domain_enum!(TileComputeStatus {
    Processed => "processed",
    SkippedNoGps => "skipped_no_gps",
    Failed => "failed",
});
domain_enum!(AnnotateStatus {
    NotProcessed => "not_processed",
    Annotated => "annotated",
    Skipped => "skipped",
    AnnotationFailed => "annotation_failed",
});

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TileSource {
    SummaryPolyline,
    StreamsClean,
    Custom(String),
}

impl TileSource {
    pub fn as_str(&self) -> &str {
        match self {
            Self::SummaryPolyline => "summary_polyline",
            Self::StreamsClean => "streams_clean",
            Self::Custom(value) => value,
        }
    }
}

impl fmt::Display for TileSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for TileSource {
    fn from(value: &str) -> Self {
        match value {
            "summary_polyline" => Self::SummaryPolyline,
            "streams_clean" => Self::StreamsClean,
            value => Self::Custom(value.into()),
        }
    }
}

impl FromStr for TileSource {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(value))
    }
}

impl FromSql for TileSource {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(Self::from(value.as_str()?))
    }
}

impl ToSql for TileSource {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_spellings_and_legacy_values_round_trip() -> rusqlite::Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        for status in [
            ActivityStatus::Pending,
            ActivityStatus::Processed,
            ActivityStatus::SkippedNoGps,
            ActivityStatus::SkippedIgnoredSport,
            ActivityStatus::Failed,
        ] {
            let stored: String = conn.query_row("SELECT ?", [status], |row| row.get(0))?;
            assert_eq!(stored, status.to_string());
            assert_eq!(stored.parse::<ActivityStatus>().unwrap(), status);
        }
        assert_eq!(
            "updated".parse::<AnnotationStatus>().unwrap(),
            AnnotationStatus::Done
        );
        let stored: String =
            conn.query_row("SELECT ?", [AnnotationStatus::Skipped], |row| row.get(0))?;
        assert_eq!(stored, "skipped");
        assert_eq!(
            stored.parse::<AnnotationStatus>().unwrap(),
            AnnotationStatus::Skipped
        );
        assert_eq!(AnnotateStatus::Skipped.to_string(), "skipped");
        assert_eq!(
            "skipped".parse::<AnnotateStatus>().unwrap(),
            AnnotateStatus::Skipped
        );
        assert_eq!(
            "squadratinho".parse::<TileKind>().unwrap(),
            TileKind::Squadratinho
        );
        assert!("other".parse::<TileKind>().is_err());
        assert_eq!(
            "skipped_no_gps"
                .parse::<TileComputeStatus>()
                .unwrap()
                .to_string(),
            "skipped_no_gps"
        );
        assert_eq!(
            "annotation_failed"
                .parse::<AnnotateStatus>()
                .unwrap()
                .to_string(),
            "annotation_failed"
        );
        for value in ["summary_polyline", "streams_clean", "", "legacy"] {
            let source: TileSource = conn.query_row("SELECT ?", [value], |row| row.get(0))?;
            assert_eq!(source.to_string(), value);
        }
        let absent: Option<TileSource> = conn.query_row("SELECT NULL", [], |row| row.get(0))?;
        assert_eq!(absent, None);
        Ok(())
    }
}
