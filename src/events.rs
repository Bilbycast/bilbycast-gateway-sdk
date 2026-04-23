// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Standard event catalog helpers — severity enum, category taxonomy,
//! and structured details. The shape must match `EventPayload` on the
//! manager side (`bilbycast-manager/crates/manager-core/src/models/ws_protocol.rs`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Event severity, in ascending order.
///
/// Wire format is lowercase (matches the manager's event storage and every
/// existing device project). Matching these strings is load-bearing — the
/// manager's severity filters key off exactly this set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSeverity {
    Info,
    Minor,
    Major,
    Critical,
}

impl EventSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            EventSeverity::Info => "info",
            EventSeverity::Minor => "minor",
            EventSeverity::Major => "major",
            EventSeverity::Critical => "critical",
        }
    }
}

/// A structured event payload. Serialises to the shape the manager's
/// `EventPayload` expects.
#[derive(Debug, Clone)]
pub struct GatewayEvent {
    pub severity: EventSeverity,
    /// Machine-readable category. Conventional taxonomy mirrors the edge/relay
    /// catalogs: `port_conflict`, `bind_failed`, `validation_error`,
    /// `connection`, `config_sync`, `vendor_api`, `auth`, `rate_limit`,
    /// plus driver-specific strings.
    pub category: String,
    pub message: String,
    /// Structured details. Convention: include an `error_code` field for any
    /// condition the manager UI should machine-match against.
    pub details: Value,
    pub flow_id: Option<String>,
    pub input_id: Option<String>,
    pub output_id: Option<String>,
}

impl GatewayEvent {
    pub fn new(
        severity: EventSeverity,
        category: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            category: category.into(),
            message: message.into(),
            details: Value::Null,
            flow_id: None,
            input_id: None,
            output_id: None,
        }
    }

    pub fn info(category: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(EventSeverity::Info, category, message)
    }

    pub fn minor(category: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(EventSeverity::Minor, category, message)
    }

    pub fn major(category: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(EventSeverity::Major, category, message)
    }

    pub fn critical(category: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(EventSeverity::Critical, category, message)
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn with_error_code(mut self, error_code: impl Into<String>) -> Self {
        let code = error_code.into();
        match &mut self.details {
            Value::Object(m) => {
                m.insert("error_code".into(), Value::String(code));
            }
            _ => {
                self.details = json!({ "error_code": code });
            }
        }
        self
    }

    pub fn with_flow_id(mut self, flow_id: impl Into<String>) -> Self {
        self.flow_id = Some(flow_id.into());
        self
    }

    pub fn with_input_id(mut self, input_id: impl Into<String>) -> Self {
        self.input_id = Some(input_id.into());
        self
    }

    pub fn with_output_id(mut self, output_id: impl Into<String>) -> Self {
        self.output_id = Some(output_id.into());
        self
    }

    /// Serialise to the JSON payload the manager expects inside the
    /// `event` envelope.
    pub fn to_payload(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("severity".into(), Value::String(self.severity.as_str().into()));
        obj.insert("category".into(), Value::String(self.category.clone()));
        obj.insert("message".into(), Value::String(self.message.clone()));
        if !matches!(self.details, Value::Null) {
            obj.insert("details".into(), self.details.clone());
        }
        if let Some(ref f) = self.flow_id {
            obj.insert("flow_id".into(), Value::String(f.clone()));
        }
        if let Some(ref i) = self.input_id {
            obj.insert("input_id".into(), Value::String(i.clone()));
        }
        if let Some(ref o) = self.output_id {
            obj.insert("output_id".into(), Value::String(o.clone()));
        }
        Value::Object(obj)
    }
}

/// Convenience: common bind-failure category names, matching the edge's
/// unified bind-failure taxonomy.
pub mod categories {
    pub const PORT_CONFLICT: &str = "port_conflict";
    pub const BIND_FAILED: &str = "bind_failed";
    pub const VALIDATION_ERROR: &str = "validation_error";
    pub const CONNECTION: &str = "connection";
    pub const CONFIG_SYNC: &str = "config_sync";
    pub const VENDOR_API: &str = "vendor_api";
    pub const AUTH: &str = "auth";
    pub const RATE_LIMIT: &str = "rate_limit";
}
