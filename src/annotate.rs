//! Strava description annotation.

use chrono::Utc;
use rusqlite::{params, Connection, TransactionBehavior};

use crate::config::Settings;
use crate::db::get_db;
use crate::descriptions::DescriptionMatcher;
use crate::error::{Error, Result};
use crate::kml_baseline::effective_tile_count_through;
use crate::models::{ActivityStatus, AnnotateStatus, AnnotationStatus, TileKind};
use crate::strava_client::{classify_strava_error, StravaApi};
use crate::sync::{
    compute_period_totals_with_conn, compute_total_unique_squadrats_through_with_conn,
    require_activity,
};

#[derive(Debug)]
pub struct AnnotateResult {
    pub status: AnnotateStatus,
    pub activity_id: i64,
    pub line: Option<String>,
    pub error: Option<String>,
}

pub fn format_thousands(n: i64) -> String {
    format_thousands_i128(i128::from(n))
}

fn format_thousands_i128(n: i128) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut formatted = String::new();
    if n < 0 {
        formatted.push('-');
    }
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

pub fn annotate_activity(
    settings: &Settings,
    api: &dyn StravaApi,
    activity_id: i64,
) -> Result<AnnotateResult> {
    annotate_activity_with_conn(
        settings,
        &mut get_db(settings)?,
        &DescriptionMatcher::new(settings),
        api,
        activity_id,
    )
}

pub(crate) fn annotate_activity_with_conn(
    settings: &Settings,
    conn: &mut Connection,
    matcher: &DescriptionMatcher,
    api: &dyn StravaApi,
    activity_id: i64,
) -> Result<AnnotateResult> {
    let row = require_activity(conn, activity_id)?;
    let mut result = AnnotateResult {
        status: AnnotateStatus::NotProcessed,
        activity_id,
        line: None,
        error: None,
    };
    if row.status != ActivityStatus::Processed {
        return Ok(result);
    }

    let has_new_tiles = row.new_squadrat_count > 0
        || (settings.include_squadratinhos_in_description && row.new_squadratinho_count > 0);
    if !has_new_tiles {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE activities
             SET annotation_status = ?, description_line = NULL, annotated_at = ?, last_error = NULL
             WHERE id = ?",
            params![
                AnnotationStatus::Skipped,
                Utc::now().to_rfc3339(),
                activity_id
            ],
        )?;
        tx.commit()?;
        result.status = AnnotateStatus::Skipped;
        return Ok(result);
    }

    let (month_total, week_total) = compute_period_totals_with_conn(conn, &row.start_local)?;
    let total = compute_total_unique_squadrats_through_with_conn(settings, conn, activity_id)?;
    let total = format_thousands_i128(i128::from(total) + i128::from(settings.squadrat_offset));
    let mut line = format!(
        "{} {}: {total} Squadrats · +{} new · +{month_total}/mo · +{week_total}/wk",
        settings.description_emoji, settings.description_prefix, row.new_squadrat_count
    );

    if settings.include_squadratinhos_in_description {
        let total = effective_tile_count_through(
            settings,
            conn,
            TileKind::Squadratinho.as_str(),
            activity_id,
            &row.start_utc,
            row.baseline_covered,
            true,
        )?;
        line.push_str(&format!(
            " · {} Squadratinhos (+{} new)",
            format_thousands(total),
            format_thousands(row.new_squadratinho_count),
        ));
    }

    let update = api.get_activity(activity_id).and_then(|activity| {
        let obj = activity.as_object().ok_or_else(|| {
            Error::Validation("Strava activity response must be a JSON object".into())
        })?;
        let current = match obj.get("description") {
            None | Some(serde_json::Value::Null) => "",
            Some(serde_json::Value::String(s)) => s.as_str(),
            Some(_) => {
                return Err(Error::Validation(
                    "Strava activity description must be a string or null".into(),
                ));
            }
        };
        let description = matcher.update(Some(current), &line);
        if description != current {
            api.update_activity_description(activity_id, &description)?;
        }
        Ok(())
    });
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match update {
        Ok(()) => {
            tx.execute(
                "UPDATE activities SET annotation_status = ?, description_line = ?, annotated_at = ?, last_error = NULL WHERE id = ?",
                params![AnnotationStatus::Done, line, Utc::now().to_rfc3339(), activity_id],
            )?;
            result.status = AnnotateStatus::Annotated;
            result.line = Some(line);
        }
        Err(error) => {
            let error = classify_strava_error(&error).to_string();
            tx.execute(
                "UPDATE activities SET annotation_status = ?, last_error = ? WHERE id = ?",
                params![AnnotationStatus::Failed, error, activity_id],
            )?;
            result.status = AnnotateStatus::AnnotationFailed;
            result.error = Some(error);
        }
    }
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_digits_without_overflow() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(1234), "1,234");
        assert_eq!(format_thousands(-1234), "-1,234");
        assert_eq!(format_thousands(i64::MIN), "-9,223,372,036,854,775,808");
    }

    #[test]
    fn writes_formatted_line_once_and_marks_annotation_done() {
        use crate::sync::tests::{database, summary, MockApi};
        use crate::sync::{compute_activity_tiles_from_summary, store_summary};

        let (_dir, mut settings) = database();
        settings.squadrat_offset = 1233;
        let conn = get_db(&settings).unwrap();
        let api = MockApi::default();
        store_summary(&settings, &conn, &summary(1, 1)).unwrap();
        compute_activity_tiles_from_summary(&settings, &api, 1).unwrap();
        let result = annotate_activity(&settings, &api, 1).unwrap();
        assert_eq!(
            result.line.as_deref(),
            Some("🗺️ TileHarvester: 1,234 Squadrats · +1 new · +1/mo · +1/wk")
        );
        annotate_activity(&settings, &api, 1).unwrap();
        assert_eq!(*api.updates.borrow(), 1);
        assert!(!settings.include_squadratinhos_in_description);
        settings.include_squadratinhos_in_description =
            Settings::from_map(&[("TH_INCLUDE_SQUADRATINHOS".into(), "true".into())].into())
                .unwrap()
                .include_squadratinhos_in_description;
        let result = annotate_activity(&settings, &api, 1).unwrap();
        assert_eq!(
            result.line.as_deref(),
            Some("🗺️ TileHarvester: 1,234 Squadrats · +1 new · +1/mo · +1/wk · 1 Squadratinhos (+1 new)")
        );
        assert_eq!(
            require_activity(&conn, 1)
                .unwrap()
                .annotation_status
                .as_str(),
            "done"
        );
    }
}
