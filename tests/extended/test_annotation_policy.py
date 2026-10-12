"""Regression coverage for annotating only rides that discover new Squadrats."""

from unittest.mock import patch

import pytest

from tileharvester.annotate import annotate_activity


@pytest.mark.parametrize("annotation_status", ["none", "failed"])
def test_zero_new_squadrats_skips_strava_and_finishes_locally(seeded_db, annotation_status):
    # Even a ride with new small tiles must not produce a '+0 new' Squadrats line.
    seeded_db.execute(
        """
        UPDATE activities
        SET new_squadrat_count = 0, new_squadratinho_count = 4,
            annotation_status = ?, last_error = 'previous failure'
        WHERE id = 2
        """,
        (annotation_status,),
    )
    seeded_db.commit()

    with (
        patch("tileharvester.annotate.get_activity") as get_activity,
        patch("tileharvester.annotate.update_activity_description") as update_description,
    ):
        result = annotate_activity(2)

    assert result == {"status": "skipped", "activity_id": 2, "reason": "no_new_squadrats"}
    get_activity.assert_not_called()
    update_description.assert_not_called()
    row = seeded_db.execute("SELECT * FROM activities WHERE id = 2").fetchone()
    assert row["annotation_status"] == "skipped"
    assert row["description_line"] is None
    assert row["annotated_at"] is not None
    assert row["last_error"] is None


def test_one_new_squadrat_still_updates_ride_description(seeded_db):
    seeded_db.execute("UPDATE activities SET new_squadrat_count = 1 WHERE id = 2")
    seeded_db.commit()

    with (
        patch("tileharvester.annotate.get_activity", return_value={"description": "Great ride!"}),
        patch("tileharvester.annotate.update_activity_description") as update_description,
    ):
        result = annotate_activity(2)

    assert result["status"] == "annotated"
    assert "+1 new" in result["line"]
    update_description.assert_called_once_with(2, "Great ride!\n\n" + result["line"])
