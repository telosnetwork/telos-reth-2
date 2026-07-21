use crate::structs::{TelosAccountStateTableRow, TelosAccountTableRow, TelosEngineApiExtraFields};
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use revm::{
    bytecode::Bytecode,
    database::State,
    primitives::AddressMap,
    state::{Account, AccountInfo, EvmStorageSlot, TransactionId},
    Database, DatabaseCommit,
};
use std::collections::HashSet;
use thiserror::Error;
use tracing::debug;

/// Summary of authoritative Telos corrections applied after local execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconciliationReport {
    /// Account records corrected or removed.
    pub accounts: usize,
    /// Storage slots corrected or removed.
    pub storage_slots: usize,
}

impl ReconciliationReport {
    /// Returns whether local execution already matched every authoritative row.
    pub const fn is_empty(self) -> bool {
        self.accounts == 0 && self.storage_slots == 0
    }
}

/// State reconciliation failed before all authoritative changes could be applied.
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
}

#[derive(Default)]
struct StateOverride {
    accounts: AddressMap<Account>,
    account_changes: HashSet<Address>,
    storage_changes: HashSet<(Address, U256)>,
}

impl StateOverride {
    fn maybe_init_account<DB: Database>(
        &mut self,
        revm_db: &mut State<DB>,
        address: Address,
    ) -> Result<(), ReconciliationError> {
        if self.accounts.contains_key(&address) {
            return Ok(())
        }
        let info = revm_db.basic(address).map_err(|err| ReconciliationError::Database {
            operation: "account",
            message: err.to_string(),
        })?;
        let mut account = match info {
            Some(info) => Account::from(info),
            None => Account::new_not_existing(TransactionId::ZERO),
        };
        account.mark_touch();
        self.accounts.insert(address, account);
        Ok(())
    }

    fn remove_account<DB: Database>(
        &mut self,
        revm_db: &mut State<DB>,
        address: Address,
    ) -> Result<(), ReconciliationError> {
        self.maybe_init_account(revm_db, address)?;
        let account = self.accounts.get_mut(&address).expect("account initialized");
        account.mark_touch();
        account.mark_selfdestruct();
        self.account_changes.insert(address);
        Ok(())
    }

    fn override_account<DB: Database>(
        &mut self,
        revm_db: &mut State<DB>,
        row: &TelosAccountTableRow,
    ) -> Result<(), ReconciliationError> {
        self.maybe_init_account(revm_db, row.address)?;
        let account = self.accounts.get_mut(&row.address).expect("account initialized");
        account.info.balance = row.balance;
        account.info.nonce = row.nonce;
        set_code(&mut account.info, &row.code);
        account.unmark_selfdestruct();
        account.mark_touch();
        self.account_changes.insert(row.address);
        Ok(())
    }

    fn override_storage<DB: Database>(
        &mut self,
        revm_db: &mut State<DB>,
        row: &TelosAccountStateTableRow,
        old_value: U256,
    ) -> Result<(), ReconciliationError> {
        self.maybe_init_account(revm_db, row.address)?;
        let account = self.accounts.get_mut(&row.address).expect("account initialized");
        let value = if row.removed { U256::ZERO } else { row.value };
        account
            .storage
            .insert(row.key, EvmStorageSlot::new_changed(old_value, value, TransactionId::ZERO));
        account.mark_touch();
        self.storage_changes.insert((row.address, row.key));
        Ok(())
    }

    fn ensure_created_account<DB: Database>(
        &mut self,
        revm_db: &mut State<DB>,
        address: Address,
    ) -> Result<(), ReconciliationError> {
        self.maybe_init_account(revm_db, address)?;
        let account = self.accounts.get_mut(&address).expect("account initialized");
        if account.info.nonce == 0 &&
            account.info.balance == U256::ZERO &&
            account.info.code_hash == KECCAK_EMPTY
        {
            account.info.nonce = 1;
            account.mark_touch();
            self.account_changes.insert(address);
        }
        Ok(())
    }

    fn apply<DB: Database>(self, revm_db: &mut State<DB>) -> ReconciliationReport {
        let report = ReconciliationReport {
            accounts: self.account_changes.len(),
            storage_slots: self.storage_changes.len(),
        };
        revm_db.commit(self.accounts);
        report
    }
}

/// Reconciles local revm state with authoritative Telos contract table deltas.
///
/// Every provider error aborts the block. Corrections are committed only after all rows have been
/// read successfully, so a partial extension cannot silently produce a partial block state.
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
    let openwallet_rows = fields
        .new_addresses_using_openwallet
        .as_ref()
        .ok_or(ReconciliationError::MissingField("new_addresses_using_openwallet"))?;

    let openwallet_addresses: HashSet<Address> =
        openwallet_rows.iter().map(|(_, value)| Address::from_word(B256::from(*value))).collect();
    let mut overrides = StateOverride::default();

    for row in account_rows {
        if row.removed {
            overrides.remove_account(revm_db, row.address)?;
            continue
        }

        if openwallet_addresses.contains(&row.address) && is_empty_row(row) {
            continue
        }

        let current = revm_db.basic(row.address).map_err(|err| ReconciliationError::Database {
            operation: "account",
            message: err.to_string(),
        })?;
        if !account_matches(current.as_ref(), row) {
            overrides.override_account(revm_db, row)?;
        }
    }

    for row in storage_rows {
        let current = revm_db.storage(row.address, row.key).map_err(|err| {
            ReconciliationError::Database { operation: "storage", message: err.to_string() }
        })?;
        let expected = if row.removed { U256::ZERO } else { row.value };
        if current != expected {
            overrides.override_storage(revm_db, row, current)?;
        }
    }

    for (_, value) in create_rows {
        overrides.ensure_created_account(revm_db, Address::from_word(B256::from(*value)))?;
    }

    let report = overrides.apply(revm_db);
    debug!(
        target: "telos::state",
        accounts = report.accounts,
        storage_slots = report.storage_slots,
        "reconciled Telos state diffs"
    );
    Ok(report)
}

/// Ensures an address allocated by the native `create` action is visible to the next EVM
/// transaction with the nonce semantics expected by the Telos contract.
pub fn prepare_created_account<DB>(
    revm_db: &mut State<DB>,
    address: Address,
) -> Result<bool, ReconciliationError>
where
    DB: Database,
    DB::Error: std::fmt::Display,
{
    let current = revm_db.basic(address).map_err(|err| ReconciliationError::Database {
        operation: "create account",
        message: err.to_string(),
    })?;
    if current.is_some() {
        return Ok(false)
    }

    let mut account = Account::new_not_existing(TransactionId::ZERO);
    account.info.nonce = 1;
    account.info.code_hash = KECCAK_EMPTY;
    account.mark_touch();
    account.mark_created();
    revm_db.commit(AddressMap::from_iter([(address, account)]));
    Ok(true)
}

fn account_matches(info: Option<&AccountInfo>, row: &TelosAccountTableRow) -> bool {
    let expected_hash = code_hash(&row.code);
    match info {
        Some(info) => {
            info.balance == row.balance &&
                info.nonce == row.nonce &&
                info.code_hash == expected_hash
        }
        None => is_empty_row(row),
    }
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

fn set_code(info: &mut AccountInfo, code: &Bytes) {
    if code.is_empty() {
        info.code_hash = KECCAK_EMPTY;
        info.code = None;
    } else {
        info.code_hash = keccak256(code);
        info.code = Some(Bytecode::new_legacy(code.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use revm::database::{CacheDB, EmptyDB};

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

    #[test]
    fn uses_ethereum_keccak_for_code_hash() {
        let address = address!("0x1000000000000000000000000000000000000000");
        let code = Bytes::from_static(&[0x60, 0x00, 0x56]);
        let mut state = state_with_account(address, AccountInfo::default());
        let row = TelosAccountTableRow { address, code: code.clone(), ..Default::default() };

        reconcile_state_diffs(&mut state, &fields(vec![row], Vec::new())).unwrap();

        assert_eq!(state.basic(address).unwrap().unwrap().code_hash, keccak256(&code));
    }

    #[test]
    fn replaces_same_length_different_code() {
        let address = address!("0x2000000000000000000000000000000000000000");
        let original = Bytes::from_static(&[0x60, 0x00]);
        let replacement = Bytes::from_static(&[0x60, 0x01]);
        let info = AccountInfo {
            code_hash: keccak256(&original),
            code: Some(Bytecode::new_legacy(original)),
            ..Default::default()
        };
        let mut state = state_with_account(address, info);
        let row = TelosAccountTableRow { address, code: replacement.clone(), ..Default::default() };

        let report = reconcile_state_diffs(&mut state, &fields(vec![row], Vec::new())).unwrap();

        assert_eq!(report.accounts, 1);
        assert_eq!(state.basic(address).unwrap().unwrap().code_hash, keccak256(&replacement));
    }

    #[test]
    fn applies_removed_accounts() {
        let address = address!("0x3000000000000000000000000000000000000000");
        let mut state = state_with_account(
            address,
            AccountInfo { balance: U256::from(10), nonce: 1, ..Default::default() },
        );
        let row = TelosAccountTableRow { removed: true, address, ..Default::default() };

        let report = reconcile_state_diffs(&mut state, &fields(vec![row], Vec::new())).unwrap();

        assert_eq!(report.accounts, 1);
        assert!(state.basic(address).unwrap().is_none());
    }

    #[test]
    fn removed_storage_is_zeroed() {
        let address = address!("0x4000000000000000000000000000000000000000");
        let key = U256::ZERO;
        let mut db = CacheDB::<EmptyDB>::default();
        db.insert_account_info(address, AccountInfo::default());
        db.insert_account_storage(address, key, U256::from(7)).unwrap();
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let row = TelosAccountStateTableRow { removed: true, address, key, value: U256::from(7) };

        reconcile_state_diffs(&mut state, &fields(Vec::new(), vec![row])).unwrap();

        assert_eq!(state.storage(address, key).unwrap(), U256::ZERO);
    }
}
