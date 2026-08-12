//! Roles, permissions and identity verification.
//!
//! Authorisation is a closed set in code rather than rows in a table. A
//! permission model stored as data is worth its cost when customers define
//! their own roles; here there are three roles fixed by what a Swedish limited
//! company's accounts actually involve, and keeping them in code means the
//! compiler checks every match and a new permission cannot be forgotten.

use serde::{Deserialize, Serialize};

/// What someone may do in one company.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The business itself. Everything, including ending the relationship.
    Owner,
    /// Works in the business. Can run analyses and read results.
    Member,
    /// An external accountant. The reason this role exists as its own thing:
    /// an advisor needs to read the accounts and the findings in order to
    /// advise, and must not be able to delete the company's data or hand access
    /// to someone else. Folding them into `Member` would grant the first;
    /// folding them into a read-only role would stop them starting the analysis
    /// they were engaged to interpret.
    Advisor,
}

/// One thing a caller may attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    ReadCompany,
    UpdateCompany,
    UploadDocument,
    ReadDocument,
    DeleteDocument,
    StartAnalysis,
    ReadAnalysis,
    ReadReport,
    ManageMembers,
    ManageTokens,
    /// Erasing the company and everything in it. Separated from
    /// `UpdateCompany` because it is the one action no amount of convenience
    /// justifies granting to an external party.
    DeleteCompany,
    ReadAuditTrail,
    /// Defining a simulation model and running it.
    ///
    /// Separate from `StartAnalysis` because the two cost different things and
    /// mean different things. An analysis reads the company's own accounts; a
    /// simulation runs arithmetic over assumptions somebody typed in, and its
    /// output is a statement about the model rather than about the company.
    RunSimulation,
    ReadSimulation,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Member => "member",
            Role::Advisor => "advisor",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Role::Owner),
            "member" => Some(Role::Member),
            "advisor" => Some(Role::Advisor),
            _ => None,
        }
    }

    /// Whether this role may do this thing.
    ///
    /// Written as an explicit match rather than as a set of bit flags so that
    /// adding a `Permission` fails to compile until every role has an answer
    /// for it. A default arm would silently grant or silently deny, and both
    /// are how a permission model rots.
    pub fn may(self, permission: Permission) -> bool {
        use Permission::*;
        match self {
            Role::Owner => true,
            Role::Member => matches!(
                permission,
                ReadCompany
                    | UpdateCompany
                    | UploadDocument
                    | ReadDocument
                    | DeleteDocument
                    | StartAnalysis
                    | ReadAnalysis
                    | ReadReport
                    | RunSimulation
                    | ReadSimulation
            ),
            Role::Advisor => matches!(
                permission,
                ReadCompany
                    | UploadDocument
                    | ReadDocument
                    | StartAnalysis
                    | ReadAnalysis
                    | ReadReport
                    // An advisor may run scenarios. It is the work they were
                    // engaged for, it touches no document they cannot already
                    // read, and it changes nothing.
                    | RunSimulation
                    | ReadSimulation
            ),
        }
    }
}

/// How confident the system is that a user is the person they claim to be.
///
/// Kept separate from both authentication and authorisation because it answers
/// a different question. Someone can hold a valid session — authenticated — and
/// have every permission their role allows — authorised — while nobody has ever
/// confirmed the human behind the account. For a product whose output feeds a
/// tax filing, that distinction has to be representable rather than assumed.
///
/// Nothing in the product currently *requires* a level above `Unverified`. The
/// axis exists so that when Swedish BankID is integrated — the obvious
/// verification method for this market — the decision is which operations
/// demand `Strong`, not how to represent verification at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationLevel {
    /// An email address that was proven reachable. Says nothing about a person.
    Unverified,
    /// A second factor is enrolled. The account is harder to take over; the
    /// human is still unconfirmed.
    TwoFactor,
    /// A national eID vouched for a named person — BankID in Sweden.
    Strong,
}

impl VerificationLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            VerificationLevel::Unverified => "unverified",
            VerificationLevel::TwoFactor => "two_factor",
            VerificationLevel::Strong => "strong",
        }
    }

    /// Whether this level satisfies a requirement.
    pub fn satisfies(self, required: VerificationLevel) -> bool {
        self >= required
    }
}

/// The answer to "may this caller do this", carrying why when the answer is no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// The role does not carry the permission.
    RoleInsufficient {
        role: Role,
        required: Permission,
    },
    /// The role is fine but identity has not been verified strongly enough.
    VerificationInsufficient {
        held: VerificationLevel,
        required: VerificationLevel,
    },
}

impl Decision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Decision::Allowed)
    }
}

/// Decides one access question.
///
/// Both axes, in one place, so a caller cannot check the role and forget the
/// verification level.
pub fn decide(
    role: Role,
    held: VerificationLevel,
    permission: Permission,
    required_verification: VerificationLevel,
) -> Decision {
    if !role.may(permission) {
        return Decision::RoleInsufficient {
            role,
            required: permission,
        };
    }
    if !held.satisfies(required_verification) {
        return Decision::VerificationInsufficient {
            held,
            required: required_verification,
        };
    }
    Decision::Allowed
}
