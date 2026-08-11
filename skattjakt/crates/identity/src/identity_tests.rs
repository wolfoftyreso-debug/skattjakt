use chrono::{Duration, Utc};

use crate::authorization::{decide, Decision, Permission, Role, VerificationLevel};
use crate::credential::{
    lockout_duration, CredentialError, PasswordPolicy, PasswordVerifier, MAX_FAILED_ATTEMPTS,
};
use crate::session::{RefreshOutcome, SessionPolicy, SessionState};
use crate::token::{ClientKind, SecretToken, TokenHash};

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[test]
fn a_token_is_never_the_same_twice() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        assert!(seen.insert(SecretToken::generate().expose().to_string()));
    }
}

#[test]
fn a_token_carries_256_bits() {
    // 32 bytes as lowercase hex.
    let token = SecretToken::generate();
    assert_eq!(token.expose().len(), 64);
    assert!(token.expose().chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn debug_does_not_print_the_token() {
    let token = SecretToken::generate();
    let printed = format!("{token:?}");
    assert!(!printed.contains(token.expose()));
    assert!(printed.contains("redacted"));
}

#[test]
fn the_hash_matches_what_a_presented_token_hashes_to() {
    let token = SecretToken::generate();
    assert_eq!(token.hash(), TokenHash::of(token.expose()));
    assert_ne!(token.hash(), TokenHash::of("something else"));
}

#[test]
fn the_hash_is_not_the_token() {
    let token = SecretToken::generate();
    assert_ne!(token.hash().as_str(), token.expose());
}

// ---------------------------------------------------------------------------
// Session policy
// ---------------------------------------------------------------------------

#[test]
fn a_browser_session_is_much_shorter_than_a_phone_session() {
    // The justification is storage: a phone has a Keychain or a Keystore, a
    // browser has storage any script on the page can read.
    let web = SessionPolicy::for_client(ClientKind::Web);
    let ios = SessionPolicy::for_client(ClientKind::Ios);
    assert!(web.refresh_lifetime < ios.refresh_lifetime);
    assert!(!ClientKind::Web.has_secure_storage());
    assert!(ClientKind::Ios.has_secure_storage());
    assert!(ClientKind::Android.has_secure_storage());
}

#[test]
fn an_access_token_always_expires_before_its_refresh_token() {
    // The database enforces this too. A session that can be used but not
    // extended is a state nothing should reach.
    for kind in [ClientKind::Web, ClientKind::Ios, ClientKind::Android] {
        let policy = SessionPolicy::for_client(kind);
        assert!(policy.access_lifetime < policy.refresh_lifetime);
    }
}

#[test]
fn an_access_token_is_short_enough_that_revocation_bites_quickly() {
    for kind in [ClientKind::Web, ClientKind::Ios, ClientKind::Android] {
        let policy = SessionPolicy::for_client(kind);
        assert!(
            policy.access_lifetime <= Duration::minutes(30),
            "a revoked session would keep working for {:?}",
            policy.access_lifetime
        );
    }
}

fn live_session(generation: i32) -> SessionState {
    SessionState {
        generation,
        refresh_expires_at: Utc::now() + Duration::days(1),
        revoked_at: None,
        rotated_at: None,
    }
}

#[test]
fn the_current_refresh_token_rotates() {
    let policy = SessionPolicy::for_client(ClientKind::Ios);
    let state = live_session(3);
    assert_eq!(
        policy.evaluate_refresh(&state, 3, Utc::now()),
        RefreshOutcome::Rotate
    );
}

#[test]
fn an_old_generation_is_reuse() {
    let policy = SessionPolicy::for_client(ClientKind::Ios);
    let state = SessionState {
        rotated_at: Some(Utc::now() - Duration::hours(1)),
        ..live_session(5)
    };
    assert_eq!(
        policy.evaluate_refresh(&state, 2, Utc::now()),
        RefreshOutcome::ReuseDetected
    );
}

#[test]
fn a_retry_inside_the_grace_window_is_not_treated_as_theft() {
    // The failure this prevents is routine on mobile: the rotation commits,
    // the response is lost, and the client retries with the only token it has.
    let policy = SessionPolicy::for_client(ClientKind::Ios);
    let state = SessionState {
        rotated_at: Some(Utc::now() - Duration::seconds(5)),
        ..live_session(4)
    };
    assert_eq!(
        policy.evaluate_refresh(&state, 3, Utc::now()),
        RefreshOutcome::ReplayWithinGrace
    );
}

#[test]
fn the_same_retry_after_the_grace_window_is_theft() {
    let policy = SessionPolicy::for_client(ClientKind::Ios);
    let state = SessionState {
        rotated_at: Some(Utc::now() - Duration::minutes(10)),
        ..live_session(4)
    };
    assert_eq!(
        policy.evaluate_refresh(&state, 3, Utc::now()),
        RefreshOutcome::ReuseDetected
    );
}

#[test]
fn a_generation_from_the_future_is_reuse() {
    // Cannot have been issued by this server: a forgery, or a sessions table
    // that was rolled back. Either deserves the family being torn down.
    let policy = SessionPolicy::for_client(ClientKind::Web);
    assert_eq!(
        policy.evaluate_refresh(&live_session(2), 9, Utc::now()),
        RefreshOutcome::ReuseDetected
    );
}

#[test]
fn an_expired_refresh_token_is_expired_not_theft() {
    let policy = SessionPolicy::for_client(ClientKind::Web);
    let state = SessionState {
        refresh_expires_at: Utc::now() - Duration::minutes(1),
        ..live_session(1)
    };
    assert_eq!(
        policy.evaluate_refresh(&state, 1, Utc::now()),
        RefreshOutcome::Expired
    );
}

#[test]
fn revocation_is_reported_ahead_of_expiry() {
    // A family revoked for reuse that later expires must still read as revoked:
    // that is the fact an operator investigating needs.
    let policy = SessionPolicy::for_client(ClientKind::Web);
    let state = SessionState {
        refresh_expires_at: Utc::now() - Duration::hours(2),
        revoked_at: Some(Utc::now() - Duration::hours(3)),
        ..live_session(1)
    };
    assert_eq!(
        policy.evaluate_refresh(&state, 1, Utc::now()),
        RefreshOutcome::Revoked
    );
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

#[test]
fn an_advisor_can_do_the_job_they_were_engaged_for() {
    // The whole reason the role exists: read the accounts and run the analysis
    // they are being paid to interpret.
    for permission in [
        Permission::ReadCompany,
        Permission::ReadDocument,
        Permission::UploadDocument,
        Permission::StartAnalysis,
        Permission::ReadAnalysis,
        Permission::ReadReport,
    ] {
        assert!(
            Role::Advisor.may(permission),
            "advisor cannot {permission:?}"
        );
    }
}

#[test]
fn an_advisor_cannot_destroy_the_relationship_or_widen_it() {
    for permission in [
        Permission::DeleteCompany,
        Permission::DeleteDocument,
        Permission::ManageMembers,
        Permission::ManageTokens,
        Permission::UpdateCompany,
        Permission::ReadAuditTrail,
    ] {
        assert!(
            !Role::Advisor.may(permission),
            "an external advisor can {permission:?}"
        );
    }
}

#[test]
fn only_an_owner_can_delete_the_company() {
    assert!(Role::Owner.may(Permission::DeleteCompany));
    assert!(!Role::Member.may(Permission::DeleteCompany));
    assert!(!Role::Advisor.may(Permission::DeleteCompany));
}

#[test]
fn only_an_owner_can_hand_out_access() {
    for permission in [Permission::ManageMembers, Permission::ManageTokens] {
        assert!(Role::Owner.may(permission));
        assert!(!Role::Member.may(permission));
        assert!(!Role::Advisor.may(permission));
    }
}

#[test]
fn every_role_can_read_the_company_it_belongs_to() {
    for role in [Role::Owner, Role::Member, Role::Advisor] {
        assert!(role.may(Permission::ReadCompany));
    }
}

#[test]
fn roles_round_trip_through_the_database_representation() {
    for role in [Role::Owner, Role::Member, Role::Advisor] {
        assert_eq!(Role::parse(role.as_str()), Some(role));
    }
    assert_eq!(Role::parse("superuser"), None);
}

#[test]
fn verification_is_ordered_and_a_higher_level_satisfies_a_lower_one() {
    assert!(VerificationLevel::Strong.satisfies(VerificationLevel::TwoFactor));
    assert!(VerificationLevel::Strong.satisfies(VerificationLevel::Unverified));
    assert!(!VerificationLevel::Unverified.satisfies(VerificationLevel::Strong));
}

#[test]
fn a_decision_says_which_axis_refused() {
    // Role and verification are different failures needing different remedies:
    // one is "ask your owner", the other is "verify with BankID".
    assert!(decide(
        Role::Owner,
        VerificationLevel::Unverified,
        Permission::DeleteCompany,
        VerificationLevel::Unverified,
    )
    .is_allowed());

    assert!(matches!(
        decide(
            Role::Advisor,
            VerificationLevel::Strong,
            Permission::DeleteCompany,
            VerificationLevel::Unverified,
        ),
        Decision::RoleInsufficient { .. }
    ));

    assert!(matches!(
        decide(
            Role::Owner,
            VerificationLevel::Unverified,
            Permission::DeleteCompany,
            VerificationLevel::Strong,
        ),
        Decision::VerificationInsufficient { .. }
    ));
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[test]
fn a_password_round_trips() {
    let verifier = PasswordVerifier::new();
    let hash = verifier.hash("räkenskapsår 2025 blir bra").unwrap();
    assert!(verifier.verify("räkenskapsår 2025 blir bra", &hash).is_ok());
    assert_eq!(
        verifier.verify("räkenskapsår 2025 blir bräa", &hash),
        Err(CredentialError::Invalid)
    );
}

#[test]
fn the_hash_is_argon2id_and_does_not_contain_the_password() {
    let verifier = PasswordVerifier::new();
    let hash = verifier.hash("ett tillräckligt långt lösenord").unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains("tillräckligt"));
}

#[test]
fn the_same_password_hashes_differently_every_time() {
    // Per-hash salt. Two customers with the same password must not have the
    // same row.
    let verifier = PasswordVerifier::new();
    let a = verifier.hash("samma lösenord som grannen").unwrap();
    let b = verifier.hash("samma lösenord som grannen").unwrap();
    assert_ne!(a, b);
    assert!(verifier.verify("samma lösenord som grannen", &a).is_ok());
    assert!(verifier.verify("samma lösenord som grannen", &b).is_ok());
}

#[test]
fn a_corrupt_stored_hash_reads_as_invalid_not_as_a_distinct_error() {
    let verifier = PasswordVerifier::new();
    assert_eq!(
        verifier.verify("anything at all", "not a hash"),
        Err(CredentialError::Invalid)
    );
}

#[test]
fn the_policy_measures_characters_not_bytes() {
    // "räkenskapsår" is 12 characters and 14 bytes. A customer told their
    // 12-character password is too short has been told something false.
    let policy = PasswordPolicy::default();
    assert!(policy.check("räkenskapsår").is_ok());
    assert!(policy.check("elva tecken").is_err());
}

#[test]
fn the_policy_rejects_the_passwords_that_are_guessed_first() {
    let policy = PasswordPolicy::default();
    assert!(policy.check("Password123!").is_err());
    assert!(policy.check("skattjakt123").is_err());
    // Case does not rescue them.
    assert!(policy.check("SKATTJAKT123").is_err());
}

#[test]
fn the_policy_accepts_a_passphrase_that_no_character_class_rule_would() {
    // The rule that forces a symbol produces "Password1!"; the passphrase is
    // stronger and would fail it.
    let policy = PasswordPolicy::default();
    assert!(policy.check("bokslut kaffe cykel oktober").is_ok());
}

#[test]
fn a_password_has_an_upper_bound() {
    // Argon2 hashes whatever it is given: an unbounded password is a denial of
    // service with a text field for an interface.
    let policy = PasswordPolicy::default();
    assert!(policy.check(&"a".repeat(257)).is_err());
    assert!(policy.check(&"a".repeat(256)).is_ok());
}

#[test]
fn lockout_grows_and_then_stops_growing() {
    let first = lockout_duration(MAX_FAILED_ATTEMPTS);
    let later = lockout_duration(MAX_FAILED_ATTEMPTS + 3);
    assert!(later > first);

    // Capped, so an attacker cannot lock a customer out permanently by failing
    // often enough — which would turn the defence into an attack.
    let extreme = lockout_duration(MAX_FAILED_ATTEMPTS + 10_000);
    assert_eq!(extreme, lockout_duration(MAX_FAILED_ATTEMPTS + 6));
    assert!(extreme <= chrono::Duration::hours(6));
}

#[test]
fn a_missing_account_costs_the_same_as_a_wrong_password() {
    // Not a timing measurement — those are flaky under a loaded CI runner.
    // What is asserted is that the equal-work path exists and runs, which is
    // the thing a refactor would silently remove.
    let verifier = PasswordVerifier::new();
    let started = std::time::Instant::now();
    verifier.spend_equivalent_work();
    assert!(
        started.elapsed() > std::time::Duration::from_micros(200),
        "the decoy verification returned too fast to have hashed anything"
    );
}
