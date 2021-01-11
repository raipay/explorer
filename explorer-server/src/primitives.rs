use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct BlockMeta {
    pub height: i32,

    pub version: i32,
    pub previous_block: [u8; 32],
    pub merkle_root: [u8; 32],
    pub timestamp: i64,
    pub bits: u32,
    pub nonce: u32,

    pub total_sats_input: i64,
    pub total_sats_output: i64,

    pub difficulty: f64,
    pub median_time: i64,
    pub size: u64,
    pub num_txs: u64,
    pub coinbase_data: Vec<u8>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TxMeta {
    pub block_height: i32,
    pub timestamp: i64,
    pub is_coinbase: bool,
    pub size: i32,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub sats_input: i64,
    pub sats_output: i64,
    pub variant: TxMetaVariant,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy)]
pub enum SlpAction {
    SlpV1Genesis = 1,
    SlpV1Mint = 2,
    SlpV1Send = 3,
    SlpNft1GroupGenesis = 4,
    SlpNft1GroupMint = 5,
    SlpNft1GroupSend = 6,
    SlpNft1UniqueChildGenesis = 7,
    SlpNft1UniqueChildSend = 8,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum TxMetaVariant {
    SatsOnly,
    Slp {
        action: SlpAction,
        token_input: u64,
        token_output: u64,
        token_id: [u8; 32],
    },
    InvalidSlp {
        token_id: Vec<u8>,
        token_input: u64,
    },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Token {
    pub token_type: u32,
    pub token_ticker: Vec<u8>,
    pub token_name: Vec<u8>,
    pub decimals: u32,
    pub group_id: Option<[u8; 32]>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Tx {
    pub version: i32,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutput>,
    pub lock_time: u32,

    pub size: u64,
    pub timestamp: i64,
    pub block_height: i32,
    pub block_hash: [u8; 32],
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TxInput {
    pub outpoint_tx_hash: [u8; 32],
    pub outpoint_out_idx: u32,
    pub signature_script: Vec<u8>,
    pub sequence: u32,
    pub sats_value: i64,
    pub previous_script: Vec<u8>,

    pub token_value: u64,
    pub is_mint_baton: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TxOutput {
    pub sats_value: i64,
    pub pubkey_script: Vec<u8>,

    pub token_value: u64,
    pub is_mint_baton: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AddressTx {
    pub timestamp: i64,
    pub block_height: i32,
    pub delta_sats: i64,
    pub delta_tokens: i64,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy)]
pub struct Utxo {
    pub sats_amount: i64,
    pub token_amount: u64,
    pub is_coinbase: bool,
    pub block_height: i32,
    pub token_id: Option<[u8; 32]>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TokenMeta {
    pub token_type: u32,
    pub token_ticker: Vec<u8>,
    pub token_name: Vec<u8>,
    pub token_document_url: Vec<u8>,
    pub token_document_hash: Vec<u8>,
    pub decimals: u32,
    pub group_id: Option<[u8; 32]>,
}