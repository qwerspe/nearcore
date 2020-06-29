use num_traits::cast::FromPrimitive;
use std::env;
use std::io;

use actix;
use bigdecimal::BigDecimal;
use diesel::{
    prelude::*,
    dsl,
    r2d2::{ConnectionManager, Pool},
};
#[macro_use]
extern crate diesel;
use tokio::sync::mpsc;
use tokio_diesel::*;
use tracing::info;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::EnvFilter;

use near_indexer;

mod models;

mod schema;

fn init_logging(verbose: bool) {
    let mut env_filter = EnvFilter::new("tokio_reactor=info,near=info,stats=info,telemetry=info");

    if verbose {
        env_filter = env_filter
            .add_directive("cranelift_codegen=warn".parse().unwrap())
            .add_directive("cranelift_codegen=warn".parse().unwrap())
            .add_directive("h2=warn".parse().unwrap())
            .add_directive("trust_dns_resolver=warn".parse().unwrap())
            .add_directive("trust_dns_proto=warn".parse().unwrap());

        env_filter = env_filter.add_directive(LevelFilter::DEBUG.into());
    } else {
        env_filter = env_filter.add_directive(LevelFilter::WARN.into());
    }

    if let Ok(rust_log) = env::var("RUST_LOG") {
        for directive in rust_log.split(',').filter_map(|s| match s.parse() {
            Ok(directive) => Some(directive),
            Err(err) => {
                eprintln!("Ignoring directive `{}`: {}", s, err);
                None
            }
        }) {
            env_filter = env_filter.add_directive(directive);
        }
    }
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(env_filter)
        .with_writer(io::stderr)
        .init();
}

async fn listen_blocks(mut stream: mpsc::Receiver<near_indexer::BlockResponse>) {
    // TODO: grab this from .env instead of hardcoding
    let manager =
        ConnectionManager::<PgConnection>::new("postgres://near:1111@localhost/near_indexer");
    let pool = Pool::builder().build(manager).unwrap();

    while let Some(block) = stream.recv().await {
        // TODO: handle data as you need
        // Block
        info!(target: "stats", "Block height {}", &block.block.header.height);
        match diesel::insert_into(schema::blocks::table)
            .values(models::Block::from_block_view(&block.block))
            .execute_async(&pool)
            .await
        {
            Ok(_) => {}
            Err(_) => continue,
        };

        // Chunks
        diesel::insert_into(schema::chunks::table)
            .values(
                block
                    .chunks
                    .iter()
                    .map(|chunk| models::Chunk::from_chunk_view(block.block.header.height, chunk))
                    .collect::<Vec<models::Chunk>>(),
            )
            .execute_async(&pool)
            .await
            .unwrap();

        // Transactions
        diesel::insert_into(schema::transactions::table)
            .values(
                block
                    .chunks
                    .iter()
                    .map(|chunk| {
                        chunk
                            .transactions
                            .iter()
                            .map(|transaction| {
                                models::Transaction::from_transaction_view(
                                    block.block.header.height,
                                    block.block.header.timestamp,
                                    transaction,
                                )
                            })
                            .collect::<Vec<models::Transaction>>()
                    })
                    .flatten()
                    .collect::<Vec<models::Transaction>>(),
            )
            .execute_async(&pool)
            .await
            .unwrap();

        // Receipts
        for chunk in block.chunks {
            for receipt in chunk.receipts {
                // Save receipt
                // Check if Receipt with given `receipt_id` is already in DB
                // Receipt might be created with one of the previous receipts
                // based on `outcome.receipt_ids` (read more below)
                let receipt_id = receipt.receipt_id.clone().to_string();
                let receipt_exists = dsl::select(
                    dsl::exists(
                        schema::receipts::table.filter(
                            schema::receipts::receipt_id.eq(receipt_id)
                        )
                    )
                )
                .get_result_async(&pool)
                .await
                .unwrap();

                if receipt_exists {
                    // Update previously created receipt with data
                    let receipt_changeset = models::Receipt::from_receipt(&receipt);
                    diesel::update(schema::receipts::table)
                        .set(receipt_changeset)
                        .execute_async(&pool)
                        .await
                        .unwrap();
                } else {
                    // Create new receipt with fulfilled data
                    diesel::insert_into(schema::receipts::table)
                        .values(
                            models::Receipt::from_receipt(&receipt)
                        )
                        .execute_async(&pool)
                        .await
                        .unwrap();
                }

                // Receipt might generate ids for future receipts
                // Inserting Receipts with filled `receipt_id` and `status` = 'empty'
                // leaving other fields as NULL
                diesel::insert_into(schema::receipts::table)
                    .values(
                        receipt.outcome.outcome.receipt_ids
                            .iter()
                            .map(|receipt_id| {
                                models::Receipt::from_receipt_id(
                                    receipt_id.to_string()
                                )
                            })
                            .collect::<Vec<models::Receipt>>()
                    )
                    .execute_async(&pool)
                    .await
                    .unwrap();

                // ReceiptData or ReceiptActions
                match &receipt.receipt {
                    ref _data @ near_indexer::near_primitives::views::ReceiptEnumView::Data { .. } => {
                        let receipt_data = models::ReceiptData::from_receipt(&receipt);
                        if let Ok(data) = receipt_data {
                            diesel::insert_into(schema::receipt_data::table)
                                .values(data)
                                .execute_async(&pool)
                                .await
                                .unwrap();
                        }
                    },
                    near_indexer::near_primitives::views::ReceiptEnumView::Action {
                            signer_id: _,
                            signer_public_key: _,
                            gas_price: _,
                            output_data_receivers,
                            input_data_ids,
                            actions
                        } => {
                            let receipt_action = models::ReceiptAction::from_receipt(&receipt);
                            if let Ok(receipt_action_) = receipt_action {
                                diesel::insert_into(schema::receipt_action::table)
                                    .values(receipt_action_)
                                    .execute_async(&pool)
                                    .await
                                    .unwrap();

                                // Input and output data
                                diesel::insert_into(schema::actions_output_data::table)
                                    .values(
                                        output_data_receivers
                                            .iter()
                                            .map(|data_receiver| models::ReceiptActionOutputData::from_data_receiver(
                                                    receipt.receipt_id.to_string().clone(),
                                                    data_receiver,
                                                ))
                                            .collect::<Vec<models::ReceiptActionOutputData>>()
                                    )
                                    .execute_async(&pool)
                                    .await
                                    .unwrap();

                                diesel::insert_into(schema::actions_input_data::table)
                                    .values(
                                        input_data_ids
                                            .iter()
                                            .map(|data_id| models::ReceiptActionInputData::from_data_id(
                                                receipt.receipt_id.to_string().clone(),
                                                data_id.to_string(),
                                            ))
                                            .collect::<Vec<models::ReceiptActionInputData>>()

                                    )
                                    .execute_async(&pool)
                                    .await
                                    .unwrap();
                            }


                            for (i, action) in actions.iter().enumerate() {
                                diesel::insert_into(schema::actions::table)
                                    .values(
                                        models::Action::from_action(
                                            receipt.receipt_id.to_string().clone(),
                                            i as i32,
                                            action,
                                        )
                                    )
                                    .execute_async(&pool)
                                    .await
                                    .unwrap();

                                // Accounts & AccessKeys
                                match action {
                                    near_indexer::near_primitives::views::ActionView::CreateAccount => {
                                        diesel::insert_into(schema::accounts::table)
                                            .values(
                                                models::Account::new(
                                                    receipt.receiver_id.to_string().clone(),
                                                    i as i32,
                                                    receipt.receipt_id.to_string().clone(),
                                                    BigDecimal::from_u64(block.block.header.timestamp).unwrap_or(0.into()),
                                                )
                                            )
                                            .execute_async(&pool)
                                            .await
                                            .unwrap();
                                    },
                                    near_indexer::near_primitives::views::ActionView::AddKey {
                                        public_key,
                                        access_key,
                                    } => {
                                        diesel::insert_into(schema::access_keys::table)
                                            .values(
                                                models::AccessKey::new(
                                                    receipt.receiver_id.to_string().clone(),
                                                    public_key.to_string(),
                                                    access_key,
                                                )
                                            )
                                            .execute_async(&pool)
                                            .await
                                            .unwrap();
                                    },
                                    _ => {},
                                };
                            }
                    }
                }

            }
        }

    }
}

fn main() {
    init_logging(false);
    let indexer = near_indexer::Indexer::new();
    let stream = indexer.receiver();
    actix::spawn(listen_blocks(stream));
    indexer.start();
}
