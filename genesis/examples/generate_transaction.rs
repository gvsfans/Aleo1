use snarkos_algorithms::merkle_tree::{MerkleParameters, MerkleTree};
use snarkos_consensus::{ConsensusParameters, GM17Verifier};
use snarkos_dpc::{
    base_dpc::{
        instantiated::Components,
        predicate::DPCPredicate,
        record_payload::PaymentRecordPayload,
        BaseDPCComponents,
        DPC,
    },
    test_data::setup_or_load_parameters,
};
use snarkos_errors::dpc::{DPCError, LedgerError};
use snarkos_models::{
    algorithms::CRH,
    dpc::DPCComponents,
    objects::{account::AccountScheme, Transaction},
};
use snarkos_objects::{Account, AccountPublicKey};
use snarkos_storage::{key_value::NUM_COLS, storage::Storage, Ledger};
use snarkos_utilities::{
    bytes::{FromBytes, ToBytes},
    to_bytes,
};

use hex;
use parking_lot::RwLock;
use rand::{thread_rng, Rng};
use snarkos_dpc::dpc::base_dpc::instantiated::Tx;
use std::{
    fs::{self, File},
    io::{Result as IoResult, Write},
    marker::PhantomData,
    path::PathBuf,
    sync::Arc,
};

/// Generate a blank ledger to facilitate generation of the genesis block
fn empty_ledger<T: Transaction, P: MerkleParameters>(
    parameters: P,
    path: &PathBuf,
) -> Result<Ledger<T, P>, LedgerError> {
    fs::create_dir_all(&path).map_err(|err| LedgerError::Message(err.to_string()))?;
    let storage = match Storage::open_cf(path, NUM_COLS) {
        Ok(storage) => storage,
        Err(err) => return Err(LedgerError::StorageError(err)),
    };

    let leaves: Vec<[u8; 32]> = vec![];
    let cm_merkle_tree = MerkleTree::<P>::new(parameters.clone(), &leaves)?;

    Ok(Ledger {
        latest_block_height: RwLock::new(0),
        storage: Arc::new(storage),
        cm_merkle_tree: RwLock::new(cm_merkle_tree),
        ledger_parameters: parameters,
        _transaction: PhantomData,
    })
}

pub fn generate(recipient: &String, balance: u64, file_name: &String) -> Result<Vec<u8>, DPCError> {
    let rng = &mut thread_rng();

    let recipient: AccountPublicKey<Components> = FromBytes::read(&hex::decode(recipient).unwrap()[..])?;
    let (ledger_parameters, parameters) = setup_or_load_parameters(false, rng);

    let predicate_vk_hash = parameters
        .circuit_parameters
        .predicate_verification_key_hash
        .hash(&to_bytes![parameters.predicate_snark_parameters.verification_key]?)?;
    let predicate_vk_hash_bytes = to_bytes![predicate_vk_hash]?;
    let predicate = DPCPredicate::<Components>::new(predicate_vk_hash_bytes.clone());

    // Generate a new account that owns the dummy input records
    let account_metadata: [u8; 32] = rng.gen();
    let dummy_account = Account::new(
        &parameters.circuit_parameters.account_signature,
        &parameters.circuit_parameters.account_commitment,
        &account_metadata,
        rng,
    )
    .unwrap();

    // Generate dummy input records

    let old_account_private_keys = vec![dummy_account.private_key.clone(); Components::NUM_INPUT_RECORDS];
    let mut old_records = vec![];
    for i in 0..Components::NUM_INPUT_RECORDS {
        let old_sn_nonce = &parameters
            .circuit_parameters
            .serial_number_nonce
            .hash(&[64u8 + (i as u8); 1])
            .unwrap();
        let old_record = DPC::generate_record(
            &parameters.circuit_parameters,
            &old_sn_nonce,
            &dummy_account.public_key,
            true, // The input record is dummy
            &PaymentRecordPayload::default(),
            &predicate,
            &predicate,
            rng,
        )
        .unwrap();
        old_records.push(old_record);
    }

    // Construct new records

    let new_payload = PaymentRecordPayload { balance, lock: 0 };

    let mut new_payloads = vec![new_payload];
    new_payloads.extend(vec![
        PaymentRecordPayload::default();
        Components::NUM_OUTPUT_RECORDS - 1
    ]);

    let new_account_public_keys = vec![recipient.clone(); Components::NUM_OUTPUT_RECORDS];
    let new_birth_predicates = vec![predicate.clone(); Components::NUM_OUTPUT_RECORDS];
    let new_death_predicates = vec![predicate.clone(); Components::NUM_OUTPUT_RECORDS];

    let mut new_dummy_flags = vec![false];
    new_dummy_flags.extend(vec![true; Components::NUM_OUTPUT_RECORDS - 1]);

    // Memo + Aux are dummies for now

    let auxiliary: [u8; 32] = rng.gen();
    let memo: [u8; 32] = rng.gen();

    // Instantiate an empty ledger

    let mut path = std::env::temp_dir();
    let random_path: usize = rng.gen();
    path.push(format!("./empty_ledger-{}", random_path));

    let ledger = empty_ledger(ledger_parameters, &path)?;

    // Generate the transaction
    let (_records, transaction) = ConsensusParameters::<GM17Verifier>::create_transaction(
        &parameters,
        old_records,
        old_account_private_keys,
        new_account_public_keys,
        new_birth_predicates,
        new_death_predicates,
        new_dummy_flags,
        new_payloads,
        auxiliary,
        memo,
        &ledger,
        rng,
    )
    .unwrap();

    let transaction_bytes = to_bytes![transaction]?;

    let size = transaction_bytes.len();
    println!("{}\n\tsize - {}", file_name, size);

    drop(ledger);
    Ledger::<Tx, <Components as BaseDPCComponents>::MerkleParameters>::destroy_storage(path).unwrap();
    Ok(transaction_bytes)
}

pub fn store(path: &PathBuf, bytes: &Vec<u8>) -> IoResult<()> {
    let mut file = File::create(path)?;
    file.write_all(&bytes)?;
    drop(file);
    Ok(())
}

pub fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 4 {
        println!(
            "Invalid number of arguments.  Given: {} - Required: {}",
            args.len() - 1,
            3
        );
        return;
    }

    let recipient = &args[1];
    let balance = args[2].parse::<u64>().unwrap();
    let file_name = &args[3];

    let bytes = generate(recipient, balance, file_name).unwrap();
    let filename = PathBuf::from(file_name);

    store(&filename, &bytes).unwrap();
}