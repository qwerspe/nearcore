//! Streamer watches the network and collects all the blocks and related chunks
//! into one struct and pushes in in to the given queue
use std::time::Duration;

use actix::Addr;
use futures::stream::StreamExt;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, info};

use near_client;
use near_crypto::{PublicKey, Signature};
pub use near_primitives::hash::CryptoHash;
pub use near_primitives::{types, views};

const INTERVAL: Duration = Duration::from_millis(500);
const INDEXER: &str = "indexer";

/// Error occurs in case of failed data fetch
#[derive(Debug)]
pub struct FailedToFetchData;

/// Resulting struct represents block with chunks
#[derive(Debug)]
pub struct BlockResponse {
    pub block: views::BlockView,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug)]
pub struct Chunk {
    pub author: types::AccountId,
    pub header: views::ChunkHeaderView,
    pub transactions: Vec<Transaction>,
    pub receipts: Vec<Receipt>,
}

#[derive(Clone, Debug)]
pub struct Receipt {
    pub predecessor_id: types::AccountId,
    pub receiver_id: types::AccountId,
    pub receipt_id: CryptoHash,
    pub outcome: views::ExecutionOutcomeWithIdView,
    pub receipt: views::ReceiptEnumView,
}

#[derive(Clone, Debug)]
pub struct Transaction {
    pub signer_id: types::AccountId,
    pub public_key: PublicKey,
    pub nonce: types::Nonce,
    pub receiver_id: types::AccountId,
    pub actions: Vec<views::ActionView>,
    pub signature: Signature,
    pub hash: CryptoHash,
    pub receipt_id: CryptoHash,
}

impl Chunk {
    pub fn from(near_chunk_view: views::ChunkView) -> Self {
        Chunk {
            author: near_chunk_view.author,
            header: near_chunk_view.header,
            transactions: vec![],
            receipts: vec![],
        }
    }
}

/// Fetches the status to retrieve `latest_block_height` to determine if we need to fetch
/// entire block or we already fetched this block.
async fn fetch_latest_block(
    client: &Addr<near_client::ViewClientActor>,
) -> Result<views::BlockView, FailedToFetchData> {
    client
        .send(near_client::GetBlock::latest())
        .await
        .map_err(|_| FailedToFetchData)?
        .map_err(|_| FailedToFetchData)
}

/// This function supposed to return the entire `BlockResponse`.
/// It calls fetches the block and fetches all the chunks for the block
/// and returns everything together in one struct
async fn fetch_block_with_chunks(
    client: &Addr<near_client::ViewClientActor>,
    block: views::BlockView,
) -> Result<BlockResponse, FailedToFetchData> {
    let chunks = fetch_chunks(&client, &block.chunks).await?;
    Ok(BlockResponse { block, chunks })
}

/// Fetches single chunk (as `near_primitives::views::ChunkView`) by provided `near_client::GetChunk` enum
async fn fetch_single_chunk(
    client: &Addr<near_client::ViewClientActor>,
    get_chunk: near_client::GetChunk,
) -> Result<views::ChunkView, FailedToFetchData> {
    client.send(get_chunk).await.map_err(|_| FailedToFetchData)?.map_err(|_| FailedToFetchData)
}


/// Fetch ExecutionOutcome for single receipt
/// Return custom Receipt struct which is a copy of near_primitives::views::ReceiptView
/// but including `outcome`
async fn fetch_single_receipt(
    client: &Addr<near_client::ViewClientActor>,
    receipt: &views::ReceiptView,
) -> Result<Receipt, FailedToFetchData> {
    let execution_outcome = client
        .send(near_client::GetExecutionOutcome {
            id: types::TransactionOrReceiptId::Receipt {
                receipt_id: receipt.receipt_id.clone(),
                receiver_id: receipt.receiver_id.clone(),
            },
        })
        .await
        .map_err(|_| FailedToFetchData)?
        .map_err(|_| FailedToFetchData)?;

    Ok(Receipt {
        outcome: execution_outcome.outcome_proof,
        predecessor_id: receipt.predecessor_id.clone(),
        receiver_id: receipt.receiver_id.clone(),
        receipt_id: receipt.receipt_id,
        receipt: receipt.receipt.clone(),
    })
}


/// Fetch ExecutionOutcome for single transaction
/// Return custom Transaction struct which is a copy of near_primitives::views::SignedTransactionView
/// but including `receipt_id` which is being took from outcome
async fn fetch_single_transaction_with_receipt_id(
    client: &Addr<near_client::ViewClientActor>,
    transaction: &views::SignedTransactionView,
) -> Result<Transaction, FailedToFetchData> {
    let execution_outcome = client
        .send(near_client::GetExecutionOutcome {
            id: types::TransactionOrReceiptId::Transaction {
                transaction_hash: transaction.hash,
                sender_id: transaction.signer_id.clone(),
            },
        })
        .await
        .map_err(|_| FailedToFetchData)?
        .map_err(|_| FailedToFetchData)?;

    Ok(Transaction {
        signer_id: transaction.signer_id.clone(),
        public_key: transaction.public_key.clone(),
        nonce: transaction.nonce.clone(),
        receiver_id: transaction.receiver_id.clone(),
        actions: transaction.actions.clone(),
        signature: transaction.signature.clone(),
        hash: transaction.hash,
        receipt_id: execution_outcome.outcome_proof.id,
    })
}

/// Fetches all the chunks by their hashes.
/// Includes transactions and receipts in custom struct (to provide more info).
/// Returns Chunks as a `Vec`
async fn fetch_chunks(
    client: &Addr<near_client::ViewClientActor>,
    chunks: &[views::ChunkHeaderView],
) -> Result<Vec<Chunk>, FailedToFetchData> {
    let chunks_hashes =
        chunks.iter().map(|chunk| near_client::GetChunk::ChunkHash(chunk.chunk_hash.into()));
    let mut chunks: futures::stream::FuturesUnordered<_> =
        chunks_hashes.map(|get_chunk| fetch_single_chunk(&client, get_chunk)).collect();
    let mut response: Vec<Chunk> = vec![];
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        let mut receipts_with_status: Vec<Receipt> = vec![];
        {
            let mut receipts: futures::stream::FuturesUnordered<_> = chunk
                .receipts
                .iter()
                .map(|receipt| fetch_single_receipt(&client, receipt))
                .collect();
            while let Some(receipt) = receipts.next().await {
                receipts_with_status.push(receipt?);
            }
        }

        let mut transactions_with_outcome: Vec<Transaction> = vec![];
        {
            let mut transactions: futures::stream::FuturesUnordered<_> = chunk
                .transactions
                .iter()
                .map(|transaction| fetch_single_transaction_with_receipt_id(&client, transaction))
                .collect();
            while let Some(transaction) = transactions.next().await {
                transactions_with_outcome.push(transaction?);
            }
        }

        let mut streamer_chunk = Chunk::from(chunk);
        streamer_chunk.receipts = receipts_with_status;
        streamer_chunk.transactions = transactions_with_outcome;
        response.push(streamer_chunk);
    }

    Ok(response)
}

/// Function that starts Streamer's busy loop. Every half a seconds it fetches the status
/// compares to already fetched block height and in case it differs fetches new block of given height.
///
/// We have to pass `client: Addr<near_client::ClientActor>` and `view_client: Addr<near_client::ViewClientActor>`.
pub async fn start(
    view_client: Addr<near_client::ViewClientActor>,
    mut queue: mpsc::Sender<BlockResponse>,
) {
    info!(target: INDEXER, "Starting Streamer...");
    let mut last_fetched_block_height: types::BlockHeight = 0;
    loop {
        time::delay_for(INTERVAL).await;
        match fetch_latest_block(&view_client).await {
            Ok(block) => {
                let latest_block_height = block.header.height;
                if latest_block_height > last_fetched_block_height {
                    last_fetched_block_height = latest_block_height;
                    info!(target: INDEXER, "The block is new");
                    let block_response =
                        fetch_block_with_chunks(&view_client, block).await.unwrap();
                    debug!(target: INDEXER, "{:#?}", &block_response);
                    match queue.send(block_response).await {
                        _ => {} // TODO: handle error somehow
                    };
                }
            }
            _ => {}
        };
    }
}
