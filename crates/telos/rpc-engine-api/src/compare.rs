use crate::structs::{TelosAccountTableRow, TelosEngineApiExtraFields};
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use revm::{
    database::State,
    primitives::AddressMap,
    state::{Account, AccountInfo, TransactionId},
    Database, DatabaseCommit,
};
use std::collections::HashSet;
use thiserror::Error;
use tracing::debug;

/// Summary of explicit native Telos effects applied after local execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconciliationReport {
    /// Native-created accounts materialized after the final EVM transaction.
    pub accounts: usize,
    /// Reserved for future explicitly specified native storage effects.
    pub storage_slots: usize,
}

impl ReconciliationReport {
    /// Returns whether local execution already matched every authoritative row.
    pub const fn is_empty(self) -> bool {
        self.accounts == 0 && self.storage_slots == 0
    }
}

/// Local execution did not exactly match the authenticated Telos execution record.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReconciliationError {
    /// The state provider returned an error.
    #[error("state provider error while reading {operation}: {message}")]
    Database {
        /// Read operation that failed.
        operation: &'static str,
        /// Provider error message.
        message: String,
    },
    /// The extension passed reconciliation without a required field.
    #[error("validated extension is missing `{0}`")]
    MissingField(&'static str),
    /// Local execution wiped storage for an account the authoritative delta keeps alive.
    #[error("local execution unexpectedly wiped storage for account {0}")]
    UnexpectedStorageWipe(Address),
    /// Local execution changed an account omitted from the authenticated account deltas.
    #[error("local execution changed account {0} without an authoritative account delta")]
    UnexpectedAccountChange(Address),
    /// Local execution changed a storage slot omitted from the authenticated storage deltas.
    #[error(
        "local execution changed storage slot {key} for account {address} without an authoritative storage delta"
    )]
    UnexpectedStorageChange {
        /// Changed account.
        address: Address,
        /// Changed storage key.
        key: U256,
    },
    /// The locally executed account does not match its authenticated final row.
    #[error("local account {0} does not match its authoritative account delta")]
    AccountMismatch(Address),
    /// The locally executed storage slot does not match its authenticated final row.
    #[error("local storage slot {key} for account {address} is {actual}, expected {expected}")]
    StorageMismatch {
        /// Mismatching account.
        address: Address,
        /// Mismatching storage key.
        key: U256,
        /// Authenticated final value.
        expected: U256,
        /// Locally executed final value.
        actual: U256,
    },
    /// A `create` event without an account row produced state other than the specified empty
    /// account with nonce one.
    #[error("native create event for {0} produced an invalid event-only account")]
    InvalidCreatedAccount(Address),
}

/// Validates local revm state against authoritative Telos contract table deltas.
///
/// Account and storage data are validation records, not state overrides; callers validate receipts
/// before invoking this function. The only state applied here is the documented nonce-one
/// materialization for a terminal native `create` event. Every provider error or mismatch aborts
/// before that native effect is committed.
pub fn reconcile_state_diffs<DB>(
    revm_db: &mut State<DB>,
    fields: &TelosEngineApiExtraFields,
) -> Result<ReconciliationReport, ReconciliationError>
where
    DB: Database,
    DB::Error: std::fmt::Display,
{
    let account_rows = fields
        .statediffs_account
        .as_ref()
        .ok_or(ReconciliationError::MissingField("statediffs_account"))?;
    let storage_rows = fields
        .statediffs_accountstate
        .as_ref()
        .ok_or(ReconciliationError::MissingField("statediffs_accountstate"))?;
    let create_rows = fields
        .new_addresses_using_create
        .as_ref()
        .ok_or(ReconciliationError::MissingField("new_addresses_using_create"))?;
    // `openwallet` is structurally required, but unlike `create` it has no persistent Ethereum
    // account effect beyond any explicit account-table row.
    let _openwallet_rows = fields
        .new_addresses_using_openwallet
        .as_ref()
        .ok_or(ReconciliationError::MissingField("new_addresses_using_openwallet"))?;

    let create_addresses: HashSet<Address> =
        create_rows.iter().map(|(_, value)| Address::from_word(B256::from(*value))).collect();
    let authoritative_accounts: HashSet<Address> =
        account_rows.iter().map(|row| row.address).collect();
    let authoritative_storage: HashSet<(Address, U256)> =
        storage_rows.iter().map(|row| (row.address, row.key)).collect();
    let removed_accounts: HashSet<Address> =
        account_rows.iter().filter(|row| row.removed).map(|row| row.address).collect();

    // Native table deltas are the complete block state transition. Local changes absent from those
    // deltas are execution divergence and must invalidate the block rather than being hidden by an
    // overlay. A create event is the sole account-only exception and has one exact permitted shape.
    let local_changes = revm_db
        .transition_state
        .as_ref()
        .map(|state| {
            state
                .transitions
                .iter()
                .map(|(address, transition)| {
                    (
                        *address,
                        transition.info.clone(),
                        transition.previous_info.clone(),
                        transition.storage_was_destroyed,
                        transition
                            .storage
                            .iter()
                            .map(|(key, slot)| (*key, slot.original_value(), slot.present_value()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut validated_event_only_creates = HashSet::new();
    for (address, current, previous, storage_was_destroyed, storage) in &local_changes {
        if *storage_was_destroyed && !removed_accounts.contains(address) {
            return Err(ReconciliationError::UnexpectedStorageWipe(*address))
        }
        if current != previous && !authoritative_accounts.contains(address) {
            if create_addresses.contains(address) &&
                previous.is_none() &&
                is_native_created_account(current.as_ref())
            {
                validated_event_only_creates.insert(*address);
            } else {
                return Err(ReconciliationError::UnexpectedAccountChange(*address))
            }
        }
        for (key, original, present) in storage {
            if original != present &&
                !authoritative_storage.contains(&(*address, *key)) &&
                !removed_accounts.contains(address)
            {
                return Err(ReconciliationError::UnexpectedStorageChange {
                    address: *address,
                    key: *key,
                })
            }
        }
    }

    for row in account_rows {
        let current = revm_db.basic(row.address).map_err(|err| ReconciliationError::Database {
            operation: "account",
            message: err.to_string(),
        })?;
        let pending_native_create = current.is_none() &&
            create_addresses.contains(&row.address) &&
            is_native_created_row(row);
        // Empty openwallet rows intentionally do not require persistence of an Ethereum empty
        // account, but any non-empty local state still has to match the supplied row.
        let matches = if row.removed {
            current.is_none()
        } else if pending_native_create {
            true
        } else {
            account_matches(revm_db, current, row)?
        };
        if !matches {
            return Err(ReconciliationError::AccountMismatch(row.address))
        }
    }

    for row in storage_rows {
        let current = revm_db.storage(row.address, row.key).map_err(|err| {
            ReconciliationError::Database { operation: "storage", message: err.to_string() }
        })?;
        // A SHIP removal is a tombstone carrying the value before deletion. Local execution can
        // therefore finish at zero (a terminal delete) or at that carried value (delete followed
        // by restore). revm intentionally omits the latter from its net transition set.
        let matches = if row.removed {
            current == U256::ZERO || current == row.value
        } else {
            current == row.value
        };
        if !matches {
            return Err(ReconciliationError::StorageMismatch {
                address: row.address,
                key: row.key,
                expected: if row.removed { U256::ZERO } else { row.value },
                actual: current,
            })
        }
    }

    let mut creates_to_apply = HashSet::new();
    for address in create_addresses {
        let current = revm_db.basic(address).map_err(|err| ReconciliationError::Database {
            operation: "create account validation",
            message: err.to_string(),
        })?;
        match current {
            None => {
                match account_rows.iter().find(|row| row.address == address) {
                    // A non-terminal create can be removed again by local execution in the same
                    // block. Its authenticated removed row is the complete final effect.
                    Some(row) if row.removed => {}
                    Some(row) if is_native_created_row(row) => {
                        creates_to_apply.insert(address);
                    }
                    Some(_) => return Err(ReconciliationError::InvalidCreatedAccount(address)),
                    None => {
                        creates_to_apply.insert(address);
                    }
                }
            }
            Some(ref info)
                if authoritative_accounts.contains(&address) ||
                    (validated_event_only_creates.contains(&address) &&
                        is_native_created_account(Some(info))) => {}
            Some(_) => return Err(ReconciliationError::InvalidCreatedAccount(address)),
        }
    }

    let report = ReconciliationReport { accounts: creates_to_apply.len(), storage_slots: 0 };
    if !creates_to_apply.is_empty() {
        revm_db.commit(AddressMap::from_iter(
            creates_to_apply.into_iter().map(|address| (address, native_created_account())),
        ));
    }
    debug!(
        target: "telos::state",
        accounts = report.accounts,
        storage_slots = report.storage_slots,
        "validated Telos state diffs and applied explicit native effects"
    );
    Ok(report)
}

/// Ensures an address allocated by the native `create` action is visible to the next EVM
/// transaction with the nonce semantics expected by the Telos contract.
pub fn prepare_created_account<DB>(
    revm_db: &mut DB,
    address: Address,
) -> Result<bool, ReconciliationError>
where
    DB: Database + DatabaseCommit,
    DB::Error: std::fmt::Display,
{
    let current = revm_db.basic(address).map_err(|err| ReconciliationError::Database {
        operation: "create account",
        message: err.to_string(),
    })?;
    if current.is_some() {
        return Ok(false)
    }

    revm_db.commit(AddressMap::from_iter([(address, native_created_account())]));
    Ok(true)
}

fn account_matches<DB: Database>(
    revm_db: &mut State<DB>,
    info: Option<AccountInfo>,
    row: &TelosAccountTableRow,
) -> Result<bool, ReconciliationError>
where
    DB::Error: std::fmt::Display,
{
    let expected_hash = code_hash(&row.code);
    let Some(info) = info else { return Ok(is_empty_row(row)) };
    if info.balance != row.balance || info.nonce != row.nonce || info.code_hash != expected_hash {
        return Ok(false)
    }
    let code = match info.code {
        Some(code) => code.original_bytes(),
        None if info.code_hash == KECCAK_EMPTY => Bytes::new(),
        None => revm_db
            .code_by_hash(info.code_hash)
            .map_err(|err| ReconciliationError::Database {
                operation: "account bytecode",
                message: err.to_string(),
            })?
            .original_bytes(),
    };
    Ok(code == row.code)
}

fn is_empty_row(row: &TelosAccountTableRow) -> bool {
    row.balance == U256::ZERO && row.nonce == 0 && row.code.is_empty()
}

fn code_hash(code: &Bytes) -> B256 {
    if code.is_empty() {
        KECCAK_EMPTY
    } else {
        keccak256(code)
    }
}

fn is_native_created_account(info: Option<&AccountInfo>) -> bool {
    info.is_some_and(|info| {
        info.balance == U256::ZERO &&
            info.nonce == 1 &&
            info.code_hash == KECCAK_EMPTY &&
            info.code.as_ref().is_none_or(|code| code.original_bytes().is_empty())
    })
}

fn is_native_created_row(row: &TelosAccountTableRow) -> bool {
    !row.removed && row.balance == U256::ZERO && row.nonce == 1 && row.code.is_empty()
}

fn native_created_account() -> Account {
    let mut account = Account::new_not_existing(TransactionId::ZERO);
    account.info.nonce = 1;
    account.info.code_hash = KECCAK_EMPTY;
    account.mark_touch();
    account.mark_created();
    account
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structs::TelosAccountStateTableRow;
    use alloy_primitives::address;
    use revm::{
        bytecode::Bytecode,
        database::{CacheDB, EmptyDB},
        state::EvmStorageSlot,
    };

    fn fields(
        accounts: Vec<TelosAccountTableRow>,
        storage: Vec<TelosAccountStateTableRow>,
    ) -> TelosEngineApiExtraFields {
        TelosEngineApiExtraFields {
            statediffs_account: Some(accounts),
            statediffs_accountstate: Some(storage),
            new_addresses_using_create: Some(Vec::new()),
            new_addresses_using_openwallet: Some(Vec::new()),
            receipts: Some(Vec::new()),
            ..Default::default()
        }
    }

    fn state_with_account(address: Address, info: AccountInfo) -> State<CacheDB<EmptyDB>> {
        let mut db = CacheDB::<EmptyDB>::default();
        db.insert_account_info(address, info);
        State::builder().with_database(db).with_bundle_update().build()
    }

    fn empty_state() -> State<CacheDB<EmptyDB>> {
        State::builder().with_database(CacheDB::<EmptyDB>::default()).with_bundle_update().build()
    }

    fn event_value(address: Address) -> U256 {
        address.into_word().into()
    }

    #[test]
    fn accepts_matching_account_code_bytes() {
        let address = address!("0x1000000000000000000000000000000000000000");
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let info = AccountInfo {
            code_hash: keccak256(&code),
            code: Some(Bytecode::new_legacy(code.clone())),
            ..Default::default()
        };
        let mut state = state_with_account(address, info);
        let row = TelosAccountTableRow { address, code: code.clone(), ..Default::default() };

        let report = reconcile_state_diffs(&mut state, &fields(vec![row], Vec::new())).unwrap();

        assert!(report.is_empty());
        assert_eq!(state.basic(address).unwrap().unwrap().code_hash, keccak256(&code));
    }

    #[test]
    fn rejects_same_length_different_code() {
        let address = address!("0x2000000000000000000000000000000000000000");
        let original = Bytes::from_static(&[0x60, 0x00]);
        let replacement = Bytes::from_static(&[0x60, 0x01]);
        let info = AccountInfo {
            code_hash: keccak256(&original),
            code: Some(Bytecode::new_legacy(original.clone())),
            ..Default::default()
        };
        let mut state = state_with_account(address, info);
        let row = TelosAccountTableRow { address, code: replacement, ..Default::default() };

        let err = reconcile_state_diffs(&mut state, &fields(vec![row], Vec::new())).unwrap_err();

        assert_eq!(err, ReconciliationError::AccountMismatch(address));
        assert_eq!(state.basic(address).unwrap().unwrap().code_hash, keccak256(&original));
    }

    #[test]
    fn rejects_account_removal_not_performed_locally() {
        let address = address!("0x3000000000000000000000000000000000000000");
        let mut state = state_with_account(
            address,
            AccountInfo { balance: U256::from(10), nonce: 1, ..Default::default() },
        );
        let row = TelosAccountTableRow { removed: true, address, ..Default::default() };

        let err = reconcile_state_diffs(&mut state, &fields(vec![row], Vec::new())).unwrap_err();

        assert_eq!(err, ReconciliationError::AccountMismatch(address));
        assert!(state.basic(address).unwrap().is_some());
    }

    #[test]
    fn accepts_removed_storage_tombstone_value_after_restore() {
        let address = address!("0x4000000000000000000000000000000000000000");
        let key = U256::ZERO;
        let mut db = CacheDB::<EmptyDB>::default();
        db.insert_account_info(address, AccountInfo::default());
        db.insert_account_storage(address, key, U256::from(7)).unwrap();
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let row = TelosAccountStateTableRow { removed: true, address, key, value: U256::from(7) };

        let report = reconcile_state_diffs(&mut state, &fields(Vec::new(), vec![row])).unwrap();

        assert!(report.is_empty());
        assert_eq!(state.storage(address, key).unwrap(), U256::from(7));
    }

    #[test]
    fn rejects_removed_storage_tombstone_value_mismatch() {
        let address = address!("0x4100000000000000000000000000000000000000");
        let key = U256::from(12);
        let mut db = CacheDB::<EmptyDB>::default();
        db.insert_account_info(address, AccountInfo::default());
        db.insert_account_storage(address, key, U256::from(8)).unwrap();
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let row = TelosAccountStateTableRow { removed: true, address, key, value: U256::from(7) };

        let err = reconcile_state_diffs(&mut state, &fields(Vec::new(), vec![row])).unwrap_err();

        assert_eq!(
            err,
            ReconciliationError::StorageMismatch {
                address,
                key,
                expected: U256::ZERO,
                actual: U256::from(8),
            }
        );
        assert_eq!(state.storage(address, key).unwrap(), U256::from(8));
    }

    #[test]
    fn rejects_local_account_changes_absent_from_authoritative_deltas() {
        let address = address!("0x5000000000000000000000000000000000000000");
        let original = AccountInfo { balance: U256::from(10), nonce: 2, ..Default::default() };
        let mut state = state_with_account(address, original.clone());
        let mut changed = Account::from(original);
        changed.info.balance = U256::from(99);
        changed.info.nonce = 3;
        changed.mark_touch();
        state.commit(AddressMap::from_iter([(address, changed)]));

        let err = reconcile_state_diffs(&mut state, &fields(Vec::new(), Vec::new())).unwrap_err();

        assert_eq!(err, ReconciliationError::UnexpectedAccountChange(address));
        let unchanged = state.basic(address).unwrap().unwrap();
        assert_eq!(unchanged.balance, U256::from(99));
        assert_eq!(unchanged.nonce, 3);
    }

    #[test]
    fn rejects_local_storage_changes_absent_from_authoritative_deltas() {
        let address = address!("0x6000000000000000000000000000000000000000");
        let key = U256::from(1);
        let mut db = CacheDB::<EmptyDB>::default();
        let info = AccountInfo { balance: U256::from(1), ..Default::default() };
        db.insert_account_info(address, info.clone());
        db.insert_account_storage(address, key, U256::from(7)).unwrap();
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut changed = Account::from(info);
        changed.mark_touch();
        changed.storage.insert(
            key,
            EvmStorageSlot::new_changed(U256::from(7), U256::from(9), TransactionId::ZERO),
        );
        state.commit(AddressMap::from_iter([(address, changed)]));

        let err = reconcile_state_diffs(&mut state, &fields(Vec::new(), Vec::new())).unwrap_err();

        assert_eq!(err, ReconciliationError::UnexpectedStorageChange { address, key });
        assert_eq!(state.storage(address, key).unwrap(), U256::from(9));
    }

    #[test]
    fn rejects_wrong_balance_nonce_and_code_for_event_only_create() {
        let address = address!("0x7000000000000000000000000000000000000000");
        let mut fields = fields(Vec::new(), Vec::new());
        fields.new_addresses_using_create = Some(vec![(0, event_value(address))]);
        let code = Bytes::from_static(&[0x60, 0x00]);
        let wrong_infos = [
            AccountInfo { balance: U256::from(99), nonce: 1, ..Default::default() },
            AccountInfo { nonce: 2, ..Default::default() },
            AccountInfo {
                nonce: 1,
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_legacy(code)),
                ..Default::default()
            },
        ];

        for info in wrong_infos {
            let mut state = empty_state();
            let mut changed = Account::new_not_existing(TransactionId::ZERO);
            changed.info = info;
            changed.mark_touch();
            changed.mark_created();
            state.commit(AddressMap::from_iter([(address, changed)]));

            let err = reconcile_state_diffs(&mut state, &fields).unwrap_err();

            assert_eq!(err, ReconciliationError::UnexpectedAccountChange(address));
        }
    }

    #[test]
    fn rejects_wrong_event_only_openwallet_account() {
        let address = address!("0x8000000000000000000000000000000000000000");
        let code = Bytes::from_static(&[0x60, 0x00]);
        let mut state = empty_state();
        let mut changed = Account::new_not_existing(TransactionId::ZERO);
        changed.info.code_hash = keccak256(&code);
        changed.info.code = Some(Bytecode::new_legacy(code));
        changed.mark_touch();
        changed.mark_created();
        state.commit(AddressMap::from_iter([(address, changed)]));
        let mut fields = fields(Vec::new(), Vec::new());
        fields.new_addresses_using_openwallet = Some(vec![(0, event_value(address))]);

        let err = reconcile_state_diffs(&mut state, &fields).unwrap_err();

        assert_eq!(err, ReconciliationError::UnexpectedAccountChange(address));
    }

    #[test]
    fn materializes_terminal_event_only_create_after_validation() {
        let address = address!("0x9000000000000000000000000000000000000000");
        let mut state = empty_state();
        let mut fields = fields(Vec::new(), Vec::new());
        fields.new_addresses_using_create = Some(vec![(0, event_value(address))]);

        let report = reconcile_state_diffs(&mut state, &fields).unwrap();

        assert_eq!(report.accounts, 1);
        let account = state.basic(address).unwrap().unwrap();
        assert_eq!(account.balance, U256::ZERO);
        assert_eq!(account.nonce, 1);
        assert_eq!(account.code_hash, KECCAK_EMPTY);
    }
}
