// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Wizard registration surface (Phase 5 of the Orchestrator → Services
//! rework).
//!
//! Lets a 3rd-party gateway vendor declare orchestration wizards alongside
//! the `CommandHandler` trait. The wizard descriptors are serialised into
//! the gateway's `register` envelope under the `wizards` field; the
//! manager merges them into its driver-registry's wizard catalog so they
//! appear in the Services Wizard Catalog UI exactly like first-party
//! (edge / relay / appear_x) wizards.
//!
//! Wire format mirrors `manager-core::drivers::WizardDescriptor` so the
//! manager can deserialise without translation. Vendors only need to:
//!
//! 1. Build a `Vec<WizardDescriptor>` for their device type.
//! 2. Implement [`WizardHandler::build_wizard_plan`] to turn an operator
//!    submission into a `Vec<PlanStep>`.
//! 3. Register both with the [`crate::GatewayClient`] at construction.
//!
//! When the manager issues a `wizard_preview` or `wizard_apply` command,
//! the SDK routes it to [`WizardHandler::build_wizard_plan`] and packs
//! the result into the standard `command_ack` shape; the manager runs
//! the steps via its existing per-node WS dispatch.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One step in a wizard's action plan. Identical wire shape to
/// `manager_core::drivers::PlanStep`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub node_id: String,
    pub description: String,
    pub action: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_entity_id: Option<String>,
}

/// Type-tagged kinds for wizard form fields. Mirrors
/// `manager_core::drivers::WizardFieldKind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WizardFieldKind {
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Multiline {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Secret {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Integer {
        min: i64,
        max: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
    },
    Bool { default: bool },
    Select {
        options: Vec<WizardSelectOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    Node {
        #[serde(default)]
        applicable_device_types: Vec<String>,
    },
    Port,
    Address {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardSelectOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardField {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub required: bool,
    #[serde(flatten)]
    pub kind: WizardFieldKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WizardDescriptor {
    pub id: String,
    pub display_label: String,
    pub category: String,
    pub description: String,
    pub fields: Vec<WizardField>,
    /// Minimum role required on every node the wizard touches. The manager
    /// checks Operator on top of this; vendors should set "operator" for
    /// any wizard that mutates the chassis.
    pub min_role: String,
    #[serde(default)]
    pub multi_node: bool,
    #[serde(default)]
    pub applicable_device_types: Vec<String>,
}

/// Errors a wizard handler may return. Mirrors `WizardError` on the
/// manager side; the SDK lifts these onto `command_ack.error_code` so the
/// manager UI can highlight the offending field.
#[derive(Debug, Clone)]
pub enum WizardError {
    UnknownWizard,
    InvalidParam {
        field: Option<String>,
        message: String,
    },
    Validation { code: String, message: String },
}

/// User-supplied wizard plan-builder. Implement this to expose
/// vendor-specific wizards in the manager's Services catalog.
#[async_trait]
pub trait WizardHandler: Send + Sync + 'static {
    /// Wizards this gateway contributes. Called once at register time and
    /// whenever the manager re-fetches metadata; cheap.
    fn wizards(&self) -> Vec<WizardDescriptor>;

    /// Build the action plan for one of this gateway's wizards.
    ///
    /// `wizard_id` matches `WizardDescriptor.id`; `params` is the
    /// operator's form submission, already schema-validated by the
    /// manager. Vendors do *semantic* validation here.
    async fn build_wizard_plan(
        &self,
        wizard_id: &str,
        params: &Value,
    ) -> Result<Vec<PlanStep>, WizardError>;
}

/// Convenience: an empty handler that registers no wizards. Vendors that
/// don't yet adopt the wizard surface can pass this and keep the rest of
/// the SDK working unchanged.
pub struct NoWizards;

#[async_trait]
impl WizardHandler for NoWizards {
    fn wizards(&self) -> Vec<WizardDescriptor> {
        vec![]
    }
    async fn build_wizard_plan(
        &self,
        _wizard_id: &str,
        _params: &Value,
    ) -> Result<Vec<PlanStep>, WizardError> {
        Err(WizardError::UnknownWizard)
    }
}
