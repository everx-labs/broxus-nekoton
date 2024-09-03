use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use ed25519_dalek::PublicKey;
use futures_util::{Future, FutureExt, Stream};
use ton_block::{MsgAddressInt, Serializable};

use nekoton_abi::{GenTimings, LastTransactionId, TransactionId};
use nekoton_utils::*;

use crate::core::models::*;
#[cfg(feature = "wallet_core")]
use crate::crypto::{SignedMessage, UnsignedMessage};
use crate::transport::models::RawTransaction;
use crate::transport::Transport;

pub fn convert_transactions(
    transactions: Vec<RawTransaction>,
) -> impl DoubleEndedIterator<Item = Transaction> {
    transactions
        .into_iter()
        .filter_map(|transaction| Transaction::try_from((transaction.hash, transaction.data)).ok())
}

pub fn request_transactions<'a>(
    transport: &'a dyn Transport,
    address: &'a MsgAddressInt,
    from_lt: u64,
    until_lt: Option<u64>,
    initial_count: u8,
    limit: Option<usize>,
) -> impl Stream<Item = Result<Vec<RawTransaction>>> + 'a {
    let initial_count = u8::min(initial_count, transport.info().max_transactions_per_fetch);
    let fut = transport.get_transactions(address, from_lt, initial_count);

    LatestTransactions {
        address,
        from_lt,
        until_lt,
        transport,
        fut: Some(fut),
        initial_count,
        total_fetched: 0,
        limit,
    }
}

#[derive(Debug)]
pub struct ParsedBlock {
    pub current_utime: u32,
    pub data: Option<(ContractState, Option<NewTransactions>)>,
}

impl ParsedBlock {
    #[inline]
    fn empty(current_utime: u32) -> Self {
        Self {
            current_utime,
            data: None,
        }
    }

    #[inline]
    fn with_data(
        current_utime: u32,
        contract_state: ContractState,
        new_transactions: Option<NewTransactions>,
    ) -> Self {
        Self {
            current_utime,
            data: Some((contract_state, new_transactions)),
        }
    }
}

pub fn parse_block(
    address: &MsgAddressInt,
    contract_state: &ContractState,
    block: &[u8],
) -> Result<ParsedBlock> {
    use ever_block::{HashmapAugType, HashmapType};
    use ton_block::Deserializable;

    let block = <ever_block::Block as ever_block::Deserializable>::construct_from_bytes(block)?;
    let info = block
        .read_info()
        .map_err(|err| BlockParsingError::InvalidBlockStructure(err.to_string()))?;

    let account_block = match block
        .read_extra()
        .and_then(|extra| extra.read_account_blocks())
        .and_then(|account_blocks| {
            account_blocks.get(&ever_block::UInt256::from_be_bytes(
                &address.address().get_bytestring(0),
            ))
        }) {
        Ok(Some(extra)) => extra,
        _ => return Ok(ParsedBlock::empty(info.gen_utime().as_u32())),
    };

    let mut balance = contract_state.balance as i128;
    let mut new_transactions = Vec::new();

    let mut last_lt = contract_state.last_lt;
    let mut latest_transaction_id: Option<TransactionId> = None;
    let mut is_deployed = contract_state.is_deployed;

    for item in account_block.transactions().iter() {
        let result = item.and_then(|(_, value)| {
            let data = ever_block::write_boc(&value.reference(0)?)?;
            let cell = ton_types::deserialize_tree_of_cells(&mut data.as_slice())?;
            let hash = cell.repr_hash();

            ton_block::Transaction::construct_from_cell(cell)
                .map(|data| RawTransaction { hash, data })
        });
        let transaction = match result {
            Ok(transaction) => transaction,
            Err(_) => continue,
        };

        balance += compute_balance_change(&transaction.data);

        is_deployed = transaction.data.end_status == ton_block::AccountStatus::AccStateActive;

        if matches!(&latest_transaction_id, Some(id) if transaction.data.lt > id.lt) {
            latest_transaction_id = Some(TransactionId {
                lt: transaction.data.lt,
                hash: transaction.hash,
            })
        }

        last_lt = std::cmp::max(last_lt, compute_account_lt(&transaction.data));
        new_transactions.push(transaction);
    }

    let new_contract_state = ContractState {
        last_lt,
        balance: balance as u64,
        gen_timings: GenTimings::Known {
            gen_lt: info.end_lt(),
            gen_utime: info.gen_utime().as_u32(),
        },
        last_transaction_id: latest_transaction_id
            .map(LastTransactionId::Exact)
            .or(contract_state.last_transaction_id),
        is_deployed,
        code_hash: contract_state.code_hash, // NOTE: code hash update is not visible
    };

    let new_transactions =
        if let (Some(first), Some(last)) = (new_transactions.first(), new_transactions.last()) {
            Some(TransactionsBatchInfo {
                min_lt: first.data.lt, // transactions in block info are in ascending order
                max_lt: last.data.lt,
                batch_type: TransactionsBatchType::New,
            })
        } else {
            None
        }
        .map(|batch_info| (new_transactions, batch_info));

    Ok(ParsedBlock::with_data(
        info.gen_utime().as_u32(),
        new_contract_state,
        new_transactions,
    ))
}

#[derive(thiserror::Error, Debug)]
pub enum BlockParsingError {
    #[error("Invalid block structure {0}")]
    InvalidBlockStructure(String),
}

type NewTransactions = (Vec<RawTransaction>, TransactionsBatchInfo);

struct LatestTransactions<'a> {
    address: &'a MsgAddressInt,
    from_lt: u64,
    until_lt: Option<u64>,
    transport: &'a dyn Transport,
    fut: Option<TransactionsFut<'a>>,
    initial_count: u8,
    total_fetched: usize,
    limit: Option<usize>,
}

#[cfg(not(feature = "non_threadsafe"))]
type TransactionsFut<'a> = Pin<Box<dyn Future<Output = Result<Vec<RawTransaction>>> + Send + 'a>>;
#[cfg(feature = "non_threadsafe")]
type TransactionsFut<'a> = Pin<Box<dyn Future<Output = Result<Vec<RawTransaction>>> + 'a>>;

impl<'a> Stream for LatestTransactions<'a> {
    type Item = Result<Vec<RawTransaction>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // poll `get_transactions` future
            let new_transactions = match self.fut.take() {
                Some(mut fut) => match fut.poll_unpin(cx) {
                    Poll::Ready(result) => result,
                    Poll::Pending => {
                        self.fut = Some(fut);
                        return Poll::Pending;
                    }
                },
                None => return Poll::Ready(None),
            };

            let mut new_transactions = match new_transactions {
                Ok(transactions) => transactions,
                // return error without resetting future
                Err(e) => return Poll::Ready(Some(Err(e))),
            };

            // ensure that transactions are sorted in reverse order (from the latest lt)
            new_transactions.sort_by_key(|tx| std::cmp::Reverse(tx.data.lt));

            // get next lt from the unfiltered response to continue
            // fetching transactions if filter produced empty array
            let next_lt_from_response = match new_transactions.last() {
                Some(last) => last.data.prev_trans_lt,
                // early return on empty response
                None => return Poll::Ready(None),
            };
            let mut possibly_has_more = next_lt_from_response > 0;

            let mut truncated = false;
            if let Some(first_tx) = new_transactions.first() {
                if first_tx.data.lt > self.from_lt {
                    // retain only elements in range (until_lt; from_lt]
                    // NOTE: `until_lt < from_lt`
                    let until_lt = self.until_lt.unwrap_or_default();
                    let range = (until_lt + 1)..=self.from_lt;

                    new_transactions.retain(|item| {
                        possibly_has_more &= item.data.lt > until_lt;
                        range.contains(&item.data.lt)
                    });
                    truncated = true;
                }
            }

            if !truncated {
                if let Some(until_lt) = self.until_lt {
                    if let Some(len) = new_transactions
                        .iter()
                        .position(|tx| tx.data.lt <= until_lt)
                    {
                        new_transactions.truncate(len);
                        possibly_has_more = false;
                    }
                }
            }

            // get batch info
            let last = match new_transactions.last() {
                Some(last) => last,
                None if possibly_has_more => {
                    self.fut = Some(self.transport.get_transactions(
                        self.address,
                        next_lt_from_response,
                        self.initial_count,
                    ));
                    continue;
                }
                None => return Poll::Ready(None),
            };

            // set next batch bound
            self.from_lt = last.data.prev_trans_lt;

            // check if there are no transactions left or all transactions were requested
            if last.data.prev_trans_lt == 0
                || matches!(self.until_lt, Some(until_lt) if last.data.prev_trans_lt <= until_lt)
            {
                return Poll::Ready(Some(Ok(new_transactions)));
            }

            // update counters
            self.total_fetched += new_transactions.len();

            let next_count = match self.limit {
                Some(limit) if self.total_fetched >= limit => {
                    return Poll::Ready(Some(Ok(new_transactions)))
                }
                Some(limit) => usize::min(
                    limit - self.total_fetched,
                    self.transport.info().max_transactions_per_fetch as usize,
                ) as u8,
                None => self.transport.info().max_transactions_per_fetch,
            };

            // If there are some unprocessed transactions left we should request remaining
            self.fut = Some(self.transport.get_transactions(
                self.address,
                last.data.prev_trans_lt,
                next_count,
            ));

            // Return result
            return Poll::Ready(Some(Ok(new_transactions)));
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct MessageContext {
    pub latest_lt: u64,
    pub created_at: u32,
    pub expire_at: u32,
}

pub trait PendingTransactionsExt {
    fn add_message(
        &mut self,
        target: &MsgAddressInt,
        message: &ton_block::Message,
        ctx: MessageContext,
    ) -> Result<PendingTransaction>;

    fn cancel(&mut self, pending_transaction: &PendingTransaction);
}

impl PendingTransactionsExt for Vec<PendingTransaction> {
    fn add_message(
        &mut self,
        target: &MsgAddressInt,
        message: &ton_block::Message,
        ctx: MessageContext,
    ) -> Result<PendingTransaction> {
        let src = match message.header() {
            ton_block::CommonMsgInfo::ExtInMsgInfo(header) => {
                if &header.dst == target {
                    None
                } else {
                    return Err(AccountSubscriptionError::InvalidMessageDestination.into());
                }
            }
            _ => return Err(AccountSubscriptionError::InvalidMessageType.into()),
        };

        let pending_transaction = PendingTransaction {
            message_hash: message.serialize()?.repr_hash(),
            src,
            latest_lt: ctx.latest_lt,
            created_at: ctx.created_at,
            expire_at: ctx.expire_at,
        };

        self.push(pending_transaction.clone());
        Ok(pending_transaction)
    }

    fn cancel(&mut self, pending_transaction: &PendingTransaction) {
        if let Some(i) = self.iter().position(|item| item.eq(pending_transaction)) {
            self.remove(i);
        }
    }
}

pub fn make_labs_unsigned_message(
    clock: &dyn Clock,
    message: ton_block::Message,
    expiration: Expiration,
    public_key: &PublicKey,
    function: Cow<'static, ton_abi::Function>,
    input: Vec<ton_abi::Token>,
) -> Result<Box<dyn UnsignedMessage>> {
    let time = clock.now_ms_u64();
    let (expire_at, header) = default_headers(time, expiration, public_key);

    let (payload, hash) =
        function.create_unsigned_call(&header, &input, false, true, message.dst())?;

    Ok(Box::new(LabsUnsignedMessage {
        function,
        header,
        input,
        payload,
        hash,
        expire_at,
        message,
    }))
}

#[derive(Clone)]
struct LabsUnsignedMessage {
    function: Cow<'static, ton_abi::Function>,
    header: HeadersMap,
    input: Vec<ton_abi::Token>,
    payload: ton_types::BuilderData,
    hash: ton_types::UInt256,
    expire_at: ExpireAt,
    message: ton_block::Message,
}

impl UnsignedMessage for LabsUnsignedMessage {
    fn refresh_timeout(&mut self, clock: &dyn Clock) {
        let time = clock.now_ms_u64();

        if !self.expire_at.refresh_from_millis(time) {
            return;
        }

        *self.header.get_mut("time").trust_me() = ton_abi::TokenValue::Time(time);
        *self.header.get_mut("expire").trust_me() = ton_abi::TokenValue::Expire(self.expire_at());

        let (payload, hash) = self
            .function
            .create_unsigned_call(&self.header, &self.input, false, true, self.message.dst())
            .trust_me();
        self.payload = payload;
        self.hash = hash;
    }

    fn expire_at(&self) -> u32 {
        self.expire_at.timestamp
    }

    fn hash(&self) -> &[u8] {
        self.hash.as_slice()
    }

    fn sign(&self, signature: &[u8; ed25519_dalek::SIGNATURE_LENGTH]) -> Result<SignedMessage> {
        let payload = self.payload.clone();
        let payload = ton_abi::Function::fill_sign(
            &self.function.abi_version,
            Some(signature),
            None,
            payload,
        )
        .and_then(ton_types::SliceData::load_builder)?;

        let mut message = self.message.clone();
        message.set_body(payload);

        Ok(SignedMessage {
            message,
            expire_at: self.expire_at(),
        })
    }

    fn sign_with_pruned_payload(
        &self,
        signature: &[u8; ed25519_dalek::SIGNATURE_LENGTH],
        prune_after_depth: u16,
    ) -> Result<SignedMessage> {
        let payload = self.payload.clone();
        let payload = ton_abi::Function::fill_sign(
            &self.function.abi_version,
            Some(signature),
            None,
            payload,
        )?
        .into_cell()?;

        let mut message = self.message.clone();
        message.set_body(prune_deep_cells(&payload, prune_after_depth)?);

        Ok(SignedMessage {
            message,
            expire_at: self.expire_at(),
        })
    }
}

pub fn default_headers(
    time: u64,
    expiration: Expiration,
    public_key: &PublicKey,
) -> (ExpireAt, HeadersMap) {
    let expire_at = ExpireAt::new_from_millis(expiration, time);

    let mut header = HashMap::with_capacity(3);
    header.insert("time".to_string(), ton_abi::TokenValue::Time(time));
    header.insert(
        "expire".to_string(),
        ton_abi::TokenValue::Expire(expire_at.timestamp),
    );
    header.insert(
        "pubkey".to_string(),
        ton_abi::TokenValue::PublicKey(Some(*public_key)),
    );

    (expire_at, header)
}

type HeadersMap = HashMap<String, ton_abi::TokenValue>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_block_v3() {
        let block = "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";
        let block = base64::decode(block).unwrap();

        let addresses = [
            "0d61258ee0270fea19348fd667946f319e3c9536f17aec5fe22b6f9a28eb04fa",
            "1a0153760b308f3ca04ffb822b0c44229c42e54ff144b292abf5b31aca4473f5",
            "1b59771eb5a0bf2997b16ddf156133a0d7e19faf953b80e56178652e39e31aec",
            "27aa8ee090386c81fcdc29dfe19eecb0d406171ae7ce9bfd421c76667305434c",
            "7019f0f628914117275516a1a0b339583291e31405ce10a61b70de8346e8ff1d",
            "7a37f904230ecf62c3756701ccc4b885b3c8483de5d81239a81528cbf6e4a3f2",
            "844855d9d9949a8f768a077e5737c1045aa0ecaee159ccd36057711eee31cf95",
            "9c979e4a506845eff6810889f1d60a14851c9f51089e9bad6ba3e6275277cec2",
            "9d54c884c5a44149fd0d6e2c36f6c635d86de28a3d48aa05fbae72ebd9183d5a",
            "cb5099c14a881a0847a97271422f498d9e53e22b173a6d6956b2c2acdd6418dc",
            "d11325a6c34136430cdb22bc02e9c40bbf6a49a1fe310c27190419bb0b8db11a",
            "de46f3fc2dfe8d951ffa5a0f6c2970841f08e09a65dcdb956fbdfcfaa71906cc",
        ];
        for address in addresses {
            let address = address.parse().unwrap();
            let address = MsgAddressInt::with_standart(None, 0, address).unwrap();
            parse_block(&address, &Default::default(), &block).unwrap();
        }
    }
}
