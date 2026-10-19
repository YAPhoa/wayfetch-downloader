//! Wire forms under the `serde` feature: identifiers and names use their
//! validated text, counters and timestamps are numbers, and enums use the
//! same snake_case names as their `Display` form. Deserializing runs the
//! constructors' validation.

use serde_json::{from_str, json, to_value};
use strum::VariantArray;

use crate::failure::FailureCode;
use crate::id::JobId;
use crate::job::{DesiredState, DisplayState, JobStage};
use crate::name::{HostName, Namespace};
use crate::time::UtcTimestamp;
use crate::version::{Generation, Revision};

#[test]
fn ids_use_text_form() {
    let id = JobId::from_u64(7).unwrap();
    assert_eq!(to_value(id).unwrap(), json!("job-7"));
    assert_eq!(from_str::<JobId>("\"job-7\"").unwrap(), id);
    assert!(from_str::<JobId>("\"pkg-7\"").is_err());
    assert!(from_str::<JobId>("7").is_err());
}

#[test]
fn counters_and_timestamps_are_numbers() {
    assert_eq!(to_value(Revision::INITIAL).unwrap(), json!(1));
    assert!(from_str::<Generation>("0").is_err());
    assert!(from_str::<Generation>("-1").is_err());
    let t = UtcTimestamp::from_unix_millis(1_500);
    assert_eq!(to_value(t).unwrap(), json!(1_500));
    assert_eq!(from_str::<UtcTimestamp>("1500").unwrap(), t);
}

#[test]
fn names_are_validated_on_input() {
    assert_eq!(
        from_str::<HostName>("\"Example.COM\"").unwrap().as_str(),
        "example.com"
    );
    assert!(from_str::<HostName>("\"https://example.com\"").is_err());
    assert!(from_str::<Namespace>("\"Bad Name\"").is_err());
}

/// Serde and strum name every variant identically.
#[test]
fn enum_wire_names_match_display() {
    for &v in JobStage::VARIANTS {
        assert_eq!(to_value(v).unwrap(), json!(v.to_string()));
        assert_eq!(
            to_value(DisplayState::Stage(v)).unwrap(),
            json!(v.to_string())
        );
    }
    for &v in DesiredState::VARIANTS {
        assert_eq!(to_value(v).unwrap(), json!(v.to_string()));
    }
    for &v in FailureCode::VARIANTS {
        assert_eq!(to_value(v).unwrap(), json!(v.to_string()));
        assert_eq!(from_str::<FailureCode>(&format!("\"{v}\"")).unwrap(), v);
    }
    assert_eq!(
        to_value(JobStage::Transferring).unwrap(),
        json!("transferring")
    );
    assert_eq!(to_value(DisplayState::Pausing).unwrap(), json!("pausing"));
    assert_eq!(
        from_str::<DisplayState>("\"stopping\"").unwrap(),
        DisplayState::Stopping
    );
    assert!(from_str::<JobStage>("\"Queued\"").is_err());
}
