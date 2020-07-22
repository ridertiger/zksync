// Built-in deps
use std::collections::VecDeque;
// External imports
use diesel::prelude::*;
use itertools::Itertools;
// Workspace imports
use models::node::{mempool::TxVariant, tx::TxHash, FranklinTx};
// Local imports
use self::records::{MempoolBatchBinding, MempoolTx, NewMempoolTx};
use crate::{schema::*, StorageProcessor};

pub mod records;

/// Schema for persisting transactions awaiting for the execution.
///
/// This schema holds the transactions that are received by the `mempool` module, but not yet have
/// been included into some block. It is required to store these transactions in the database, so
/// in case of the unexpected server reboot sent transactions won't disappear, and will be executed
/// as if the server haven't been relaunched.
#[derive(Debug)]
pub struct MempoolSchema<'a>(pub &'a StorageProcessor);

impl<'a> MempoolSchema<'a> {
    /// Loads all the transactions stored in the mempool schema.
    pub fn load_txs(&self) -> Result<VecDeque<TxVariant>, failure::Error> {
        fn get_batch_id(batch: &Option<MempoolBatchBinding>) -> Option<i64> {
            batch.as_ref().map(|batch| batch.batch_id)
        }

        // Load the transactions from mempool along with corresponding batch IDs.
        let query = "SELECT * FROM mempool_txs \
                     LEFT JOIN mempool_batch_binding ON mempool_txs.id = mempool_tx_id
                     ORDER BY mempool_txs.id";

        let txs: Vec<(MempoolTx, Option<MempoolBatchBinding>)> =
            diesel::sql_query(query).load(self.0.conn())?;

        let mut prev_batch_id = txs.first().map(|(_, batch)| get_batch_id(batch)).flatten();

        let grouped_txs = txs.into_iter().group_by(|(_, batch)| {
            prev_batch_id = get_batch_id(&batch);

            prev_batch_id
        });

        let mut txs = VecDeque::new();

        for (batch_id, group) in grouped_txs.into_iter() {
            let deserialized_txs: Vec<FranklinTx> = group
                .map(|(tx_object, _)| serde_json::from_value(tx_object.tx).map_err(From::from))
                .collect::<Result<Vec<FranklinTx>, failure::Error>>()?;

            match batch_id {
                Some(_) => {
                    // Group of batched transactions.
                    let variant = TxVariant::from(deserialized_txs);
                    txs.push_back(variant);
                }
                None => {
                    // Group of non-batched transactions.
                    let mut variants = deserialized_txs.into_iter().map(TxVariant::from).collect();
                    txs.append(&mut variants);
                }
            }
        }

        Ok(txs)
    }

    /// Adds a new transaction to the mempool schema.
    pub fn insert_tx(&self, tx_data: &FranklinTx) -> Result<(), failure::Error> {
        let tx_hash = hex::encode(tx_data.hash().as_ref());
        let tx = serde_json::to_value(tx_data)?;

        let db_entry = NewMempoolTx { tx_hash, tx };

        diesel::insert_into(mempool_txs::table)
            .values(db_entry)
            .execute(self.0.conn())?;

        Ok(())
    }

    pub fn remove_tx(&self, tx: &[u8]) -> QueryResult<()> {
        let tx_hash = hex::encode(tx);

        diesel::delete(mempool_txs::table.filter(mempool_txs::tx_hash.eq(&tx_hash)))
            .execute(self.0.conn())?;

        // TODO: Check if there is a corresponding batch for the tx, and remove it as well if necessary.

        Ok(())
    }

    fn remove_txs(&self, txs: &[TxHash]) -> Result<(), failure::Error> {
        let tx_hashes: Vec<_> = txs.iter().map(hex::encode).collect();

        diesel::delete(mempool_txs::table.filter(mempool_txs::tx_hash.eq_any(&tx_hashes)))
            .execute(self.0.conn())?;

        Ok(())
    }

    /// Removes transactions that are already committed.
    /// Though it's unlikely that mempool schema will ever contain a committed
    /// transaction, it's better to ensure that we won't process the same transaction
    /// again. One possible scenario for having already-processed txs in the database
    /// is a failure of `remove_txs` method, which won't cause a panic on server, but will
    /// left txs in the database.
    ///
    /// This method is expected to be initially invoked on the server start, and then
    /// invoked periodically with a big interval (to prevent possible database bloating).
    pub fn collect_garbage(&self) -> Result<(), failure::Error> {
        let mut txs_to_remove: Vec<_> = self.load_txs()?.into_iter().collect();
        txs_to_remove.retain(|tx| {
            match tx {
                TxVariant::Tx(tx) => {
                    let tx_hash = tx.hash();
                    self.0
                        .chain()
                        .operations_ext_schema()
                        .get_tx_by_hash(tx_hash.as_ref())
                        .expect("DB issue while restoring the mempool state")
                        .is_some()
                }
                TxVariant::Batch(_batch) => {
                    // TODO
                    unimplemented!()
                }
            }
        });

        let tx_hashes: Vec<_> = txs_to_remove
            .into_iter()
            .map(|tx| tx.hashes())
            .flatten()
            .collect();

        self.remove_txs(&tx_hashes)?;

        Ok(())
    }
}
