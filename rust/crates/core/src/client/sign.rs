//! Sign and submit Solana transactions supplied as base64.

use base64::Engine;
use solana_mpp::solana_keychain::SolanaSigner;
use solana_signature::Signature;
use solana_transaction::{Transaction, versioned::VersionedTransaction};

use crate::{Error, Result};

/// Production entrypoint for `pay sign`.
pub fn sign_and_submit_base64_transaction(
    transaction_base64: &str,
    network: &str,
    account_override: Option<&str>,
) -> Result<String> {
    let store = crate::accounts::FileAccountsStore::default_path();
    let intent = crate::keystore::AuthIntent::use_account("sign and submit a Solana transaction");
    let (signer, _) = crate::signer::load_signer_for_network_with_intent(
        network,
        &store,
        account_override,
        &intent,
    )?;
    let rpc_url = std::env::var("PAY_RPC_URL")
        .unwrap_or_else(|_| solana_mpp::protocol::solana::default_rpc_url(network).to_string());
    let submitter = RpcTransactionSubmitter { rpc_url };
    let rt = signing_runtime()?;

    rt.block_on(sign_and_submit_with_submitter(
        transaction_base64,
        &signer,
        &submitter,
    ))
}

fn signing_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Config(format!("Failed to create signing runtime: {e}")))
}

trait TransactionSubmitter {
    fn submit(&self, transaction: &VersionedTransaction) -> Result<String>;
}

struct RpcTransactionSubmitter {
    rpc_url: String,
}

impl TransactionSubmitter for RpcTransactionSubmitter {
    fn submit(&self, transaction: &VersionedTransaction) -> Result<String> {
        use solana_mpp::solana_rpc_client::rpc_client::RpcClient;

        RpcClient::new(self.rpc_url.clone())
            .send_and_confirm_transaction(transaction)
            .map(|signature| signature.to_string())
            .map_err(|e| Error::Mpp(format!("transaction submission failed: {e}")))
    }
}

async fn sign_and_submit_with_submitter(
    transaction_base64: &str,
    signer: &dyn SolanaSigner,
    submitter: &dyn TransactionSubmitter,
) -> Result<String> {
    let transaction = decode_base64_transaction(transaction_base64)?;
    let transaction = sign_transaction(transaction, signer).await?;
    submitter.submit(&transaction)
}

fn decode_base64_transaction(transaction_base64: &str) -> Result<VersionedTransaction> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(transaction_base64)
        .map_err(|e| Error::Config(format!("Invalid base64 Solana transaction: {e}")))?;

    match bincode::deserialize::<Transaction>(&bytes) {
        Ok(transaction) => Ok(VersionedTransaction::from(transaction)),
        Err(legacy_error) => {
            bincode::deserialize::<VersionedTransaction>(&bytes).map_err(|versioned_error| {
                Error::Config(format!(
                    "Invalid Solana transaction bytes: legacy decode failed ({legacy_error}); \
                     versioned decode failed ({versioned_error})"
                ))
            })
        }
    }
}

async fn sign_transaction(
    mut transaction: VersionedTransaction,
    signer: &dyn SolanaSigner,
) -> Result<VersionedTransaction> {
    transaction
        .sanitize()
        .map_err(|e| Error::Config(format!("Invalid Solana transaction: {e}")))?;

    let signer_pubkey = signer.pubkey();
    let required_signatures = usize::from(transaction.message.header().num_required_signatures);
    let required_signer_keys = transaction
        .message
        .static_account_keys()
        .get(..required_signatures)
        .ok_or_else(|| {
            Error::Config(
                "Invalid Solana transaction: required signer keys are missing".to_string(),
            )
        })?;
    let signature_index = required_signer_keys
        .iter()
        .position(|pubkey| pubkey == &signer_pubkey)
        .ok_or_else(|| {
            Error::Config(format!(
                "Selected pay account `{signer_pubkey}` is not a required signer for this transaction"
            ))
        })?;

    let signature_bytes = signer
        .sign_message(&transaction.message.serialize())
        .await
        .map_err(|e| Error::Mpp(format!("transaction signing failed: {e}")))?;
    transaction.signatures[signature_index] = Signature::from(<[u8; 64]>::from(signature_bytes));

    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use solana_hash::Hash;
    use solana_instruction::{AccountMeta, Instruction};
    use solana_message::{Message, VersionedMessage, v0};
    use solana_mpp::solana_keychain::MemorySigner;
    use solana_pubkey::Pubkey;

    const TEST_SIGNATURE: &str =
        "2ZpY3vMZ7qQX3sRzWpe4bFzgXpaT7HbFoR4ZeYqgZcK8BBcrXWUzj2KCQh1qQdnRuy5uXWgqN65YQHT7ycs44Dq";

    struct FakeSubmitter {
        result: Result<String>,
        submitted: Mutex<Vec<VersionedTransaction>>,
    }

    impl FakeSubmitter {
        fn succeeds() -> Self {
            Self {
                result: Ok(TEST_SIGNATURE.to_string()),
                submitted: Mutex::new(Vec::new()),
            }
        }

        fn fails() -> Self {
            Self {
                result: Err(Error::Mpp("fake RPC failure".to_string())),
                submitted: Mutex::new(Vec::new()),
            }
        }

        fn submitted(&self) -> Vec<VersionedTransaction> {
            self.submitted.lock().unwrap().clone()
        }
    }

    impl TransactionSubmitter for FakeSubmitter {
        fn submit(&self, transaction: &VersionedTransaction) -> Result<String> {
            self.submitted.lock().unwrap().push(transaction.clone());
            match &self.result {
                Ok(signature) => Ok(signature.clone()),
                Err(error) => Err(Error::Mpp(error.to_string())),
            }
        }
    }

    #[test]
    fn signs_and_submits_legacy_base64_transaction() {
        let signer = test_signer(1);
        let submitter = FakeSubmitter::succeeds();
        let transaction = legacy_transaction(&signer.pubkey(), &[]);

        let signature = block_on(sign_and_submit_with_submitter(
            &encode(&transaction),
            &signer,
            &submitter,
        ))
        .unwrap();

        assert_eq!(signature, TEST_SIGNATURE);
        let submitted = submitter.submitted();
        assert_eq!(submitted.len(), 1);
        assert_ne!(submitted[0].signatures[0], Signature::default());
    }

    #[test]
    fn signs_and_submits_v0_base64_transaction() {
        let signer = test_signer(2);
        let submitter = FakeSubmitter::succeeds();
        let transaction = v0_transaction(&signer.pubkey(), &[]);

        let signature = block_on(sign_and_submit_with_submitter(
            &encode(&transaction),
            &signer,
            &submitter,
        ))
        .unwrap();

        assert_eq!(signature, TEST_SIGNATURE);
        let submitted = submitter.submitted();
        assert!(matches!(submitted[0].message, VersionedMessage::V0(_)));
        assert_ne!(submitted[0].signatures[0], Signature::default());
    }

    #[test]
    fn preserves_existing_signatures_outside_pay_signer_slot() {
        let signer = test_signer(3);
        let co_signer = test_signer(4);
        let existing_signature = Signature::from([9; 64]);
        let mut transaction = legacy_transaction(
            &signer.pubkey(),
            &[AccountMeta::new(co_signer.pubkey(), true)],
        );
        transaction.signatures[1] = existing_signature;
        let submitter = FakeSubmitter::succeeds();

        block_on(sign_and_submit_with_submitter(
            &encode(&transaction),
            &signer,
            &submitter,
        ))
        .unwrap();

        let submitted = submitter.submitted();
        assert_ne!(submitted[0].signatures[0], Signature::default());
        assert_eq!(submitted[0].signatures[1], existing_signature);
    }

    #[test]
    fn rejects_account_not_required_by_transaction() {
        let signer = test_signer(5);
        let other_signer = test_signer(6);
        let submitter = FakeSubmitter::succeeds();
        let transaction = legacy_transaction(&other_signer.pubkey(), &[]);

        let error = block_on(sign_and_submit_with_submitter(
            &encode(&transaction),
            &signer,
            &submitter,
        ))
        .unwrap_err()
        .to_string();

        assert!(error.contains("not a required signer"));
        assert!(submitter.submitted().is_empty());
    }

    #[test]
    fn rejects_invalid_base64_before_submit() {
        let signer = test_signer(7);
        let submitter = FakeSubmitter::succeeds();

        let error = block_on(sign_and_submit_with_submitter(
            "not-base64",
            &signer,
            &submitter,
        ))
        .unwrap_err()
        .to_string();

        assert!(error.contains("Invalid base64 Solana transaction"));
        assert!(submitter.submitted().is_empty());
    }

    #[test]
    fn rejects_invalid_transaction_bytes_before_submit() {
        let signer = test_signer(8);
        let submitter = FakeSubmitter::succeeds();
        let payload = base64::engine::general_purpose::STANDARD.encode([1, 2, 3]);

        let error = block_on(sign_and_submit_with_submitter(
            &payload, &signer, &submitter,
        ))
        .unwrap_err()
        .to_string();

        assert!(error.contains("Invalid Solana transaction bytes"));
        assert!(submitter.submitted().is_empty());
    }

    #[test]
    fn propagates_submit_failure() {
        let signer = test_signer(9);
        let submitter = FakeSubmitter::fails();
        let transaction = legacy_transaction(&signer.pubkey(), &[]);

        let error = block_on(sign_and_submit_with_submitter(
            &encode(&transaction),
            &signer,
            &submitter,
        ))
        .unwrap_err()
        .to_string();

        assert!(error.contains("fake RPC failure"));
        assert_eq!(submitter.submitted().len(), 1);
    }

    #[test]
    fn signing_runtime_supports_blocking_rpc_client() {
        let rt = signing_runtime().unwrap();

        rt.block_on(async {
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            );
        });
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn test_signer(seed: u8) -> MemorySigner {
        let secret = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let public = secret.verifying_key();
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&secret.to_bytes());
        bytes.extend_from_slice(public.as_bytes());
        MemorySigner::from_bytes(&bytes).unwrap()
    }

    fn instruction(accounts: &[AccountMeta]) -> Instruction {
        Instruction {
            program_id: Pubkey::new_from_array([11; 32]),
            accounts: accounts.to_vec(),
            data: vec![1, 2, 3],
        }
    }

    fn legacy_transaction(payer: &Pubkey, accounts: &[AccountMeta]) -> Transaction {
        let message = Message::new_with_blockhash(
            &[instruction(accounts)],
            Some(payer),
            &Hash::new_from_array([12; 32]),
        );
        Transaction::new_unsigned(message)
    }

    fn v0_transaction(payer: &Pubkey, accounts: &[AccountMeta]) -> VersionedTransaction {
        let message = v0::Message::try_compile(
            payer,
            &[instruction(accounts)],
            &[],
            Hash::new_from_array([13; 32]),
        )
        .unwrap();
        VersionedTransaction {
            signatures: vec![
                Signature::default();
                usize::from(message.header.num_required_signatures)
            ],
            message: VersionedMessage::V0(message),
        }
    }

    fn encode<T: serde::Serialize>(transaction: &T) -> String {
        base64::engine::general_purpose::STANDARD.encode(bincode::serialize(transaction).unwrap())
    }
}
