use alloy::network::{EthereumWallet, ReceiptResponse};
use alloy::primitives::{Address, TxHash, U256, address};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use clap::Parser;
use std::env;
use std::str::FromStr;
use tokio::time::{Duration, Instant};
use zksync_os_alloy_ext::network::Zksync;
use zksync_os_alloy_ext::provider::ZksyncApi;
use zksync_os_contract_interface::Bridgehub;

#[derive(Parser, Debug)]
#[command(version, about = "Finalize an L2->L1 ETH withdrawal on L1")]
struct Args {
    /// L2 withdrawal transaction hash
    #[arg(short = 't', long)]
    tx_hash: TxHash,

    /// L1 RPC URL (default: $L1_RPC_URL or http://localhost:8545)
    #[arg(long)]
    l1_rpc_url: Option<String>,

    /// L2 RPC URL (default: $L2_RPC_URL or http://localhost:3050)
    #[arg(long)]
    l2_rpc_url: Option<String>,

    /// Private key for the L1 wallet (default: $DEPLOYER_PRIVATE_KEY)
    #[arg(long)]
    private_key: Option<String>,
}

alloy::sol! {
    #[sol(rpc)]
    interface IL1AssetRouter {
        IL1Nullifier public immutable L1_NULLIFIER;
    }

    #[sol(rpc)]
    interface IL1Nullifier {
        struct FinalizeL1DepositParams {
            uint256 chainId;
            uint256 l2BatchNumber;
            uint256 l2MessageIndex;
            address l2Sender;
            uint16 l2TxNumberInBatch;
            bytes message;
            bytes32[] merkleProof;
        }

        function finalizeDeposit(FinalizeL1DepositParams calldata _finalizeWithdrawalParams) external;
    }

    interface IL1Messenger {
        event L1MessageSent(address indexed _sender, bytes32 indexed _hash, bytes _message);
    }
}

const L1_MESSENGER_ADDRESS: Address = address!("0000000000000000000000000000000000008008");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let l1_rpc_url = args.l1_rpc_url
        .or_else(|| env::var("L1_RPC_URL").ok())
        .unwrap_or_else(|| "http://localhost:8545".to_owned());
    let l2_rpc_url = args.l2_rpc_url
        .or_else(|| env::var("L2_RPC_URL").ok())
        .unwrap_or_else(|| "http://localhost:3050".to_owned());
    let private_key = args.private_key
        .or_else(|| env::var("DEPLOYER_PRIVATE_KEY").ok())
        .ok_or_else(|| anyhow::anyhow!("--private-key or $DEPLOYER_PRIVATE_KEY required"))?;

    // --- L1 provider (with wallet for sending finalizeDeposit) ---
    let wallet = EthereumWallet::new(LocalSigner::from_str(&private_key)?);
    let l1_provider = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect(&l1_rpc_url)
        .await?;

    // --- L2 provider (Zksync network, read-only) ---
    let l2_provider = ProviderBuilder::new_with_network::<Zksync>()
        .connect(&l2_rpc_url)
        .await?;

    // --- Fetch L2 withdrawal receipt ---
    println!("Fetching L2 receipt for {}...", args.tx_hash);
    let l2_receipt = l2_provider
        .get_transaction_receipt(args.tx_hash)
        .await?
        .ok_or_else(|| anyhow::anyhow!("L2 transaction not found: {}", args.tx_hash))?;

    anyhow::ensure!(
        l2_receipt.status(),
        "L2 withdrawal transaction reverted — cannot finalize"
    );

    // --- Resolve L1AssetRouter via Bridgehub ---
    let bridgehub_address = l2_provider.get_bridgehub_contract().await?;
    let chain_id = l2_provider.get_chain_id().await?;
    let bridgehub = Bridgehub::new(bridgehub_address, &l1_provider, chain_id);
    let shared_bridge_address = bridgehub.shared_bridge_address().await?;
    let l1_asset_router = IL1AssetRouter::new(shared_bridge_address, &l1_provider);
    let l1_nullifier_address = l1_asset_router.L1_NULLIFIER().call().await?;
    let l1_nullifier = IL1Nullifier::new(l1_nullifier_address, &l1_provider);

    println!("L1AssetRouter : {shared_bridge_address}");
    println!("L1Nullifier   : {l1_nullifier_address}");

    // --- Extract L1MessageSent from L2 receipt logs ---
    let l1_message_sent = l2_receipt
        .logs()
        .iter()
        .find_map(|log| {
            if log.address() != L1_MESSENGER_ADDRESS {
                return None;
            }
            log.log_decode::<IL1Messenger::L1MessageSent>().ok()
        })
        .ok_or_else(|| anyhow::anyhow!("no L1MessageSent event in withdrawal receipt — is this an ETH/base-token withdrawal?"))?;

    // --- Find the matching L2->L1 system log ---
    let (l2_to_l1_log_index, l2_to_l1_log) = l2_receipt
        .inner
        .l2_to_l1_logs()
        .iter()
        .enumerate()
        .find(|(_, log)| log.sender == L1_MESSENGER_ADDRESS)
        .ok_or_else(|| anyhow::anyhow!("no L2->L1 log from L1Messenger in withdrawal receipt"))?;

    // --- Poll for Merkle proof (batch must be executed on L1 first) ---
    println!("Waiting for L2->L1 log proof (up to 120s)...");
    let proof_timeout = Duration::from_secs(120);
    let proof_delay = Duration::from_secs(2);
    let started_at = Instant::now();
    let proof = loop {
        let elapsed = started_at.elapsed();
        if elapsed >= proof_timeout {
            anyhow::bail!("node did not provide proof within {proof_timeout:?} — is the L2 batch executed on L1?");
        }
        let remaining = proof_timeout - elapsed;

        match tokio::time::timeout(
            remaining,
            l2_provider.get_l2_to_l1_log_proof(args.tx_hash, l2_to_l1_log_index as u64),
        )
        .await
        {
            Ok(Ok(Some(proof))) => {
                println!("  Got proof: batch={}, id={}", proof.batch_number, proof.id);
                break proof;
            }
            Ok(Ok(None)) | Ok(Err(_)) => {
                println!("  Proof not ready yet, retrying...");
                tokio::time::sleep(proof_delay).await;
            }
            Err(_) => anyhow::bail!("timeout waiting for proof"),
        }
    };

    // l2_to_l1_log.key: for user messages the sender address is stored right-padded in bytes 12..32
    let sender = Address::from_slice(&l2_to_l1_log.key[12..]);

    // --- Print L1 balance before ---
    let finalize_sender = wallet.default_signer().address();
    let balance_before = l1_provider.get_balance(finalize_sender).await?;
    println!(
        "L1 balance before: {:.6} ETH",
        balance_before.to::<u128>() as f64 / 1e18
    );

    // --- Call finalizeDeposit on L1Nullifier ---
    println!("Submitting finalizeDeposit on L1...");
    let l1_tx_receipt = l1_nullifier
        .finalizeDeposit(IL1Nullifier::FinalizeL1DepositParams {
            chainId: U256::from(chain_id),
            l2BatchNumber: U256::from(proof.batch_number),
            l2MessageIndex: U256::from(proof.id),
            l2Sender: sender,
            l2TxNumberInBatch: l2_receipt.transaction_index.unwrap().try_into().unwrap(),
            message: l1_message_sent.inner.data._message,
            merkleProof: proof.proof,
        })
        .send()
        .await?
        .get_receipt()
        .await?;

    anyhow::ensure!(
        l1_tx_receipt.status(),
        "finalizeDeposit reverted — tx: {}",
        l1_tx_receipt.transaction_hash()
    );

    println!("L1 finalization tx: {}", l1_tx_receipt.transaction_hash());

    let balance_after = l1_provider.get_balance(finalize_sender).await?;
    println!(
        "L1 balance after : {:.6} ETH",
        balance_after.to::<u128>() as f64 / 1e18
    );

    Ok(())
}
