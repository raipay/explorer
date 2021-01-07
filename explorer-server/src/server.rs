use anyhow::{anyhow, Result};
use bitcoin_cash::{Address, Script};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use warp::Reply;
use serde::Serialize;
use chrono::{Utc, TimeZone};
use chrono_humanize::HumanTime;
use std::{borrow::Cow, collections::{HashMap, hash_map::Entry}, convert::TryInto};

use crate::{blockchain::{BlockHeader, Destination, destination_from_script, from_le_hex, is_coinbase, to_le_hex}, db::{Db, SlpAction, TokenMeta, TxMeta, TxMetaVariant, TxOutSpend}, formatting::{render_amount, render_byte_size, render_difficulty, render_integer, render_sats}, grpc::{AddressBalance, Bchd, bchrpc}};

pub struct Server {
    bchd: Bchd,
    satoshi_addr_prefix: &'static str,
    tokens_addr_prefix: &'static str,
}

impl Server {
    pub async fn setup(db: Db) -> Result<Self> {
        let satoshi_addr_prefix = "bitcoincash";
        let bchd = Bchd::connect(db, satoshi_addr_prefix).await?;
        Ok(Server {
            bchd,
            satoshi_addr_prefix,
            tokens_addr_prefix: "simpleledger",
        })
    }
}

impl Server {
    pub async fn dashboard(&self) -> Result<impl Reply> {
        let blockchain_info = self.bchd.blockchain_info().await?;
        let page_size = 2000;
        let current_page_height = (blockchain_info.best_height / page_size) * page_size;
        let current_page_end = blockchain_info.best_height;
        let last_page_height = current_page_height - page_size;
        let last_page_end = current_page_height - 1;
        let markup = html! {
            (DOCTYPE)
            head {
                meta charset="utf-8";
                title { "be.cash Block Explorer" }
                script
                    src="https://code.jquery.com/jquery-3.1.1.min.js"
                    integrity="sha256-hVVnYaiADRTO2PzUGmuLJr8BLUSjGIZsDYGmIJLv2b8="
                    crossorigin="anonymous" {}
                script type="text/javascript" src="code/semantic-ui/semantic.js?v=0" {}
                script type="text/javascript" src="code/webix/webix.js?v=8.1.0" {}
                script type="text/javascript" src="code/moment.min.js?v=0" {}
                link rel="stylesheet" href="code/webix/webix.css";
                link rel="stylesheet" href="code/semantic-ui/semantic.css";
                link rel="stylesheet" href="code/styles/index.css";
                link rel="preconnect" href="https://fonts.gstatic.com";
                link href="https://fonts.googleapis.com/css2?family=Ubuntu+Mono&display=swap" rel="stylesheet";
            }
            body {
                (self.toolbar())

                #blocks {
                    #blocks-table {}
                }
                
                script type="text/javascript" src={"/data/blocks/" (current_page_height) "/" (current_page_end) "/dat.js"} {}
                script type="text/javascript" src={"/data/blocks/" (last_page_height) "/" (last_page_end) "/dat.js"} {}
                script type="text/javascript" src="/code/blocks.js" {}
            }
        };
        Ok(warp::reply::html(markup.into_string()))
    }

    pub async fn blocks(&self, start_height: i32, _end_height: i32) -> Result<impl Reply> {
        let blocks = self.bchd.blocks_above(start_height - 1).await?;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Block {
            hash: String,
            height: i32,

            version: i32,
            timestamp: i64,

            difficulty: f64,
            size: u64,
            num_txs: u64,
            median_time: i64,
        }
        let mut json_blocks = Vec::with_capacity(blocks.len());
        for block in blocks.into_iter().rev() {
            json_blocks.push(Block {
                hash: to_le_hex(&block.block_info.hash),
                height: block.block_info.height,
                version: block.block_info.version,
                timestamp: block.block_info.timestamp,
                difficulty: block.block_info.difficulty,
                size: block.block_meta.size,
                median_time: block.block_meta.median_time,
                num_txs: block.block_meta.num_txs,
            });
        }
        let encoded_blocks = serde_json::to_string(&json_blocks)?;
        let reply = format!(r#"
            if (window.blockData === undefined)
                window.blockData = [];
            {{
                var blocks = JSON.parse('{encoded_blocks}');
                var startIdx = window.blockData.length;
                window.blockData.length += blocks.length;
                for (var i = 0; i < blocks.length; ++i) {{
                    var block = blocks[i];
                    window.blockData[startIdx + i] = {{
                        hash: block.hash,
                        height: block.height,
                        version: block.version,
                        timestamp: new Date(block.timestamp * 1000),
                        difficulty: block.difficulty,
                        size: block.size,
                        medianTime: block.medianTime,
                        numTxs: block.numTxs,
                    }};
                }}
            }}
        "#, encoded_blocks = encoded_blocks);
        let reply = warp::reply::with_header(reply, "content-type", "application/javascript");
        let reply = warp::reply::with_header(reply, "last-modified", "Tue, 29 Dec 2020 06:31:27 GMT");
        Ok(reply)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonTx {
    tx_hash: String,
    block_height: Option<i32>,
    timestamp: i64,
    is_coinbase: bool,
    size: i32,
    num_inputs: u32,
    num_outputs: u32,
    sats_input: i64,
    sats_output: i64,
    token_idx: Option<usize>,
    is_burned_slp: bool,
    token_input: u64,
    token_output: u64,
    slp_action: Option<SlpAction>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonToken {
    token_id: String,
    token_type: u32,
    token_ticker: String,
    token_name: String,
    decimals: u32,
    group_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonTxs {
    txs: Vec<JsonTx>,
    tokens: Vec<JsonToken>,
    token_indices: HashMap<[u8; 32], usize>,
}

impl Server {
    pub async fn block_txs(&self, block_hash: &str) -> Result<impl Reply> {
        let block_hash = from_le_hex(block_hash)?;
        let block_txs = self.bchd.block_txs(&block_hash).await?;
        let json_txs = self.json_txs(
            block_txs.iter()
                .map(|(tx_hash, tx_meta)| (tx_hash.as_slice(), 0, Some(tx_meta.block_height), tx_meta))
        ).await?;
        let encoded_txs = serde_json::to_string(&json_txs.txs)?;
        let encoded_tokens = serde_json::to_string(&json_txs.tokens)?;
        let reply = format!(r#"
            if (window.txData === undefined)
                window.txData = [];
            {{
                var txs = JSON.parse('{encoded_txs}');
                var tokens = JSON.parse('{encoded_tokens}');
                var startIdx = window.txData.length;
                window.txData.length += txs.length;
                for (var i = 0; i < txs.length; ++i) {{
                    var tx = txs[i];
                    tx.token = tx.tokenIdx === null ? null : tokens[tx.tokenIdx];
                    window.txData[startIdx + i] = tx;
                }}
            }}
        "#, encoded_txs = encoded_txs, encoded_tokens = encoded_tokens);
        let reply = warp::reply::with_header(reply, "content-type", "application/javascript");
        let reply = warp::reply::with_header(reply, "last-modified", "Tue, 29 Dec 2020 06:31:27 GMT");
        Ok(reply)
    }

    async fn json_txs(&self, txs: impl ExactSizeIterator<Item=(&[u8], i64, Option<i32>, &TxMeta)>) -> Result<JsonTxs> {
        let mut json_txs = Vec::with_capacity(txs.len());
        let mut token_indices = HashMap::<[u8; 32], usize>::new();
        for (tx_hash, timestamp, block_height, tx_meta) in txs {
            let mut tx = JsonTx {
                tx_hash: to_le_hex(&tx_hash),
                block_height,
                timestamp,
                is_coinbase: tx_meta.is_coinbase,
                size: tx_meta.size,
                num_inputs: tx_meta.num_inputs,
                num_outputs: tx_meta.num_outputs,
                sats_input: tx_meta.sats_input,
                sats_output: tx_meta.sats_output,
                token_idx: None,
                is_burned_slp: false,
                token_input: 0,
                token_output: 0,
                slp_action: None,
            };
            let mut tx_token_id = None;
            match &tx_meta.variant {
                TxMetaVariant::Normal => {},
                TxMetaVariant::InvalidSlp { token_id, token_input } => {
                    tx_token_id = Some(token_id);
                    tx.is_burned_slp = true;
                    tx.token_input = *token_input;
                }
                TxMetaVariant::Slp { token_id, token_input, token_output, action } => {
                    tx_token_id = Some(token_id);
                    tx.token_input = *token_input;
                    tx.token_output = *token_output;
                    tx.slp_action = Some(*action);
                }
            }
            if let Some(&token_id) = tx_token_id {
                let num_tokens = token_indices.len();
                match token_indices.entry(token_id) {
                    Entry::Vacant(vacant) => {
                        vacant.insert(num_tokens);
                        tx.token_idx = Some(num_tokens);
                    },
                    Entry::Occupied(occupied) => {
                        tx.token_idx = Some(*occupied.get());
                    }
                }
            }
            json_txs.push(tx);
        }
        let tokens = self.bchd.tokens(token_indices.keys().map(|key| &key[..])).await?;
        let mut token_data = tokens.into_iter().zip(&token_indices).collect::<Vec<_>>();
        token_data.sort_unstable_by_key(|&(_, (_, idx))| idx);
        let json_tokens = token_data.into_iter().map(|(token_meta, (token_id, _))| {
            let token_ticker = String::from_utf8_lossy(&token_meta.token_ticker);
            let token_name = String::from_utf8_lossy(&token_meta.token_name);
            JsonToken {
                token_id: hex::encode(token_id),
                token_type: token_meta.token_type,
                token_ticker: html! { (token_ticker) }.into_string(),
                token_name: html! { (token_name) }.into_string(),
                decimals: token_meta.decimals,
                group_id: token_meta.group_id.map(|group_id| hex::encode(&group_id)),
            }
        }).collect::<Vec<_>>();
        Ok(JsonTxs { tokens: json_tokens, txs: json_txs, token_indices })
    }

    pub async fn block(&self, block_hash_str: &str) -> Result<impl Reply> {
        let block_hash = from_le_hex(block_hash_str)?;
        let block_meta_info = self.bchd.block_meta_info(&block_hash).await?;
        let block_info = block_meta_info.block_info;
        let block_meta = block_meta_info.block_meta;
        let timestamp = Utc.timestamp(block_info.timestamp, 0);
        let mut block_header = BlockHeader::default();
        block_header.version = block_info.version;
        block_header.previous_block = block_info.previous_block.as_slice().try_into()?;
        block_header.merkle_root = block_info.merkle_root.as_slice().try_into()?;
        block_header.timestamp = block_info.timestamp.try_into()?;
        block_header.bits = block_info.bits;
        block_header.nonce = block_info.nonce;
        
        let markup = html! {
            (DOCTYPE)
            head {
                title { "be.cash Block Explorer" }
                (self.head_common())
            }
            body {
                (self.toolbar())

                .ui.container {
                    h1 {
                        "Block #"
                        (block_info.height)
                    }
                    .ui.segment {
                        strong { "Hash: " }
                        span.hex { (block_hash_str) }
                    }
                    .ui.grid {
                        .six.wide.column {
                            table.ui.table {
                                tbody {
                                    tr {
                                        td { "Age" }
                                        td { (HumanTime::from(timestamp)) }
                                    }
                                    tr {
                                        td { "Mined on" }
                                        td { (self.render_timestamp(block_info.timestamp)) }
                                    }
                                    tr {
                                        td { "Unix Timestamp" }
                                        td { (render_integer(block_info.timestamp as u64)) }
                                    }
                                    tr {
                                        td { "Mined by" }
                                        td { "Unknown" }
                                    }
                                    tr {
                                        td { "Confirmations" }
                                        td { (block_info.confirmations) }
                                    }
                                    tr {
                                        td { "Size" }
                                        td { (render_byte_size(block_meta.size, true)) }
                                    }
                                    tr {
                                        td { "Transactions" }
                                        td { (block_meta.num_txs) }
                                    }
                                }
                            }
                        }
                        .ten.wide.column {
                            table.ui.table {
                                tbody {
                                    tr {
                                        td { "Difficulty" }
                                        td { (render_difficulty(block_info.difficulty)) }
                                    }
                                    tr {
                                        td { "Header" }
                                        td {
                                            .hex {
                                                (hex::encode(block_header.as_slice()))
                                            }
                                        }
                                    }
                                    tr {
                                        td { "Nonce" }
                                        td { (block_info.nonce) }
                                    }
                                    tr {
                                        td { "Coinbase data" }
                                        td {
                                            (String::from_utf8_lossy(&block_meta.coinbase_data))
                                        }
                                    }
                                    tr {
                                        td { "Coinbase hex" }
                                        td {
                                            .hex {
                                                (hex::encode(&block_meta.coinbase_data))
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    .ui.segment {
                        h2.ui.header { "Transactions" }
                        #txs-table {}
                    }
                }
                script type="text/javascript" src={"/data/block/" (block_hash_str) "/dat.js"} {}
                script type="text/javascript" src="/code/txs.js" {}
            }
        };
        Ok(warp::reply::html(markup.into_string()))
    }

    pub async fn tx(&self, tx_hash_str: &str) -> Result<impl Reply> {
        let tx_hash = from_le_hex(tx_hash_str)?;
        let tx = self.bchd.tx(&tx_hash).await?;
        let tx = tx.unwrap();
        let title: Cow<str> = match tx.tx_meta.variant {
            TxMetaVariant::Normal => "ABC Transaction".into(),
            TxMetaVariant::InvalidSlp {..} => "Invalid SLP Transaction".into(),
            TxMetaVariant::Slp {..} => {
                let token_meta = tx.token_meta.as_ref().ok_or_else(|| anyhow!("No token meta"))?;
                format!("{} Token Transaction", String::from_utf8_lossy(&token_meta.token_ticker)).into()
            }
        };
        let block_meta_info = self.bchd.block_meta_info(&tx.transaction.block_hash).await?;
        let block_info = block_meta_info.block_info;
        let timestamp = Utc.timestamp(block_info.timestamp, 0);
        let markup = html! {
            (DOCTYPE)
            head {
                title { "be.cash Block Explorer" }
                (self.head_common())
            }
            body {
                (self.toolbar())

                .ui.container {
                    h1 { (title) }
                    #tx-hash.ui.segment {
                        strong { "Transaction ID: " }
                        span.hex { (tx_hash_str) }
                        @if tx.tx_meta.is_coinbase {
                            .ui.green.horizontal.label { "Coinbase" }
                        }
                        .ui.slider.checkbox style="float: right;" {
                            (PreEscaped(
                                r#"<script type="text/javascript">
                                    function toggleHex() {
                                        if ($('#raw-hex-toggle').is(':checked')) {
                                            $('#raw-hex').show()
                                        } else {
                                            $('#raw-hex').hide()
                                        }
                                    }
                                </script>"#,
                            ))
                            input#raw-hex-toggle
                                type="checkbox"
                                onclick="toggleHex()";
                            label { "Show raw hex" }
                        }
                    }
                    #raw-hex.ui.segment style="display: none;" {
                        h4 { "Raw Transaction Hex" }
                        .hex {
                            (hex::encode(&tx.raw_tx))
                        }
                    }
                    .ui.grid {
                        .six.wide.column {
                            table.ui.table {
                                tbody {
                                    tr {
                                        td { "Age" }
                                        td { (HumanTime::from(timestamp)) }
                                    }
                                    tr {
                                        td { "Mined on" }
                                        td { (self.render_timestamp(block_info.timestamp)) }
                                    }
                                    tr {
                                        td { "Unix Timestamp" }
                                        td { (render_integer(block_info.timestamp as u64)) }
                                    }
                                    tr {
                                        td { "Block" }
                                        td {
                                            a href={"/block/" (to_le_hex(&tx.transaction.block_hash))} {
                                                (render_integer(tx.transaction.block_height as u64))
                                            }
                                            " ("
                                            (render_integer(block_info.confirmations as u64))
                                            " confirmations)"
                                        }
                                    }
                                    tr {
                                        td { "Size" }
                                        td { (render_byte_size(tx.transaction.size as u64, true)) }
                                    }
                                    tr {
                                        td { "Total Input" }
                                        td { (render_sats(tx.tx_meta.sats_input, false)) " ABC" }
                                    }
                                    tr {
                                        td { "Total Output" }
                                        td { (render_sats(tx.tx_meta.sats_output, false)) " ABC" }
                                    }
                                    tr {
                                        td { "Version" }
                                        td { (tx.transaction.version) }
                                    }
                                    tr {
                                        td { "Locktime" }
                                        td { (render_integer(tx.transaction.lock_time as u64)) }
                                    }
                                }
                            }
                        }
                        .ten.wide.column {
                            (self.render_tx_variant(&tx.tx_meta.variant, &tx.token_meta))
                        }
                    }
                    .ui.grid {
                        .eight.wide.column {
                            h2 { "Inputs" }
                            (PreEscaped(
                                r#"<script type="text/javascript">
                                    var detailsOpen = {};
                                    function toggleDetails(kind, idx) {{
                                        var key = kind + idx
                                        if (detailsOpen[key]) {{
                                            $('#' + kind + '-details-' + idx).hide();
                                            $('#' + kind + '-details-toggle-' + idx).removeClass('up').addClass('down');
                                        }} else {{
                                            $('#' + kind + '-details-' + idx).show();
                                            $('#' + kind + '-details-toggle-' + idx).removeClass('down').addClass('up');
                                        }}
                                        detailsOpen[key] = !detailsOpen[key];
                                    }}
                                </script>"#,
                            ))
                            table#inputs.ui.table {
                                tbody {
                                    @for input in &tx.transaction.inputs {
                                        (self.render_input(input, &tx.token_meta))
                                    }
                                }
                            }
                        }
                        .eight.wide.column {
                            h2 { "Outputs" }
                            table#outputs.ui.table {
                                tbody {
                                    @for output in &tx.transaction.outputs {
                                        (self.render_output(output, &tx.token_meta, &tx.tx_out_spends))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        Ok(warp::reply::html(markup.into_string()))
    }

    fn render_tx_variant(&self, variant: &TxMetaVariant, token_meta: &Option<TokenMeta>) -> Markup {
        use SlpAction::*;
        match (*variant, token_meta) {
            (
                TxMetaVariant::Slp { token_id, action, token_input, token_output },
                Some(token_meta),
            ) => html! {
                @let ticker = String::from_utf8_lossy(&token_meta.token_ticker);
                @let action_str = match action {
                    SlpV1Genesis => "GENESIS",
                    SlpV1Mint => "MINT",
                    SlpV1Send => "SEND",
                    SlpNft1GroupGenesis => "NFT1 Group GENESIS",
                    SlpNft1GroupMint => "NFT1 MINT",
                    SlpNft1GroupSend => "NFT1 Group SEND",
                    SlpNft1UniqueChildGenesis => "NFT1 Child GENESIS",
                    SlpNft1UniqueChildSend => "NFT1 Child SEND",
                };
                h2 {
                    a href={"/tx/" (hex::encode(&token_id))} { (ticker) }
                    " Token " (action_str) " Transaction"
                }
                table.ui.table {
                    tbody {
                        tr {
                            td { "Token ID" }
                            td { .hex { (hex::encode(&token_id)) } }
                        }
                        tr {
                            td { "Token Name" }
                            td { (String::from_utf8_lossy(&token_meta.token_name)) }
                        }
                        tr {
                            td { "Token Type" }
                            td {
                                @match token_meta.token_type {
                                    0x01 => {
                                        "Type1 ("
                                        a href="https://github.com/simpleledger/slp-specifications/blob/master/slp-token-type-1.md" {
                                            "Specification"
                                        }
                                        ")"
                                    }
                                    0x41 => {
                                        "NFT1 Child ("
                                        a href="https://github.com/simpleledger/slp-specifications/blob/master/slp-nft-1.md" {
                                            "Specification"
                                        }
                                        ")"
                                    }
                                    0x81 => {
                                        "NFT1 Group ("
                                        a href="https://github.com/simpleledger/slp-specifications/blob/master/slp-nft-1.md" {
                                            "Specification"
                                        }
                                        ")"
                                    }
                                    token_type => { "Unknown type: " (token_type) }
                                }
                            }
                        }
                        tr {
                            td { "Transaction Type" }
                            td { (action_str) }
                        }
                        tr {
                            td { "Token Output" }
                            td {
                                (render_amount(token_output, token_meta.decimals)) " " (ticker)
                                @if token_output < token_input {
                                    br;
                                    " ("
                                    (render_amount(token_input - token_output, token_meta.decimals))
                                    " " (ticker) " burned)"
                                }
                            }
                        }
                        tr {
                            td { "Document URI" }
                            td {
                                @let token_url = String::from_utf8_lossy(&token_meta.token_document_url);
                                a href={(token_url)} target="_blank" { (token_url) }
                            }
                        }
                        tr {
                            td { "Document Hash" }
                            td {
                                @match token_meta.token_document_url.len() {
                                    0 => .ui.black.horizontal.label { "Not set" },
                                    _ => .hex { (hex::encode(&token_meta.token_document_hash)) },
                                }
                            }
                        }
                        tr {
                            td { "Decimals" }
                            td { (token_meta.decimals) }
                        }
                    }
                }
            },
            (
                TxMetaVariant::InvalidSlp { token_id, token_input },
                Some(token_meta)
            ) => html! {
                @let ticker = String::from_utf8_lossy(&token_meta.token_ticker);
                h2 {
                    "Invalid Token Transaction ("
                    (ticker)
                    ")"
                }
                table.ui.table {
                    tbody {
                        tr {
                            td { "Token ID" }
                            td { .hex { (hex::encode(&token_id)) } }
                        }
                        tr {
                            td { "Token Name" }
                            td { (String::from_utf8_lossy(&token_meta.token_name)) }
                        }
                        tr {
                            td { "Tokens burned" }
                            td {
                                (render_amount(token_input, token_meta.decimals)) " " (ticker)
                            }
                        }
                    }
                }
            },
            (
                TxMetaVariant::InvalidSlp { token_id, token_input },
                None
            ) => html! {
                h2 {
                    "Invalid Token Transaction (unknown token)"
                }
                table.ui.table {
                    tbody {
                        tr {
                            td { "Token ID" }
                            td { .hex { (hex::encode(&token_id)) } }
                        }
                        tr {
                            td { "Tokens burned" }
                            td {
                                (render_integer(token_input))
                            }
                        }
                    }
                }
            },
            _ => html! {},
        }
    }

    pub fn render_output(
        &self,
        tx_output: &bchrpc::transaction::Output,
        token_meta: &Option<TokenMeta>,
        tx_out_spends: &HashMap<u32, Option<TxOutSpend>>,
    ) -> Markup {
        let is_token = tx_output.slp_token.as_ref().map(|slp| slp.amount > 0 || slp.is_mint_baton).unwrap_or(false);
        let destination = destination_from_script(
            if is_token { self.tokens_addr_prefix } else { self.satoshi_addr_prefix },
            &tx_output.pubkey_script,
        );
        let output_script = Script::deser_ops(tx_output.pubkey_script.as_slice().into())
            .map(|script| script.to_string())
            .unwrap_or("invalid script".to_string());
        html! {
            tr {
                td {
                    @match tx_out_spends.get(&tx_output.index) {
                        Some(Some(tx_out_spend)) => {
                            a href={"/tx/" (to_le_hex(&tx_out_spend.by_tx_hash))} {
                                img src={"/assets/spend.svg"} {}
                            }
                        }
                        Some(None) => {
                            img src={"/assets/utxo.svg"} {}
                        }
                        None => {
                            @if let Destination::Nulldata(_) = &destination {
                                i.icon.sticky.note.outline {}
                            } @else {
                                i.icon.question {}
                            }
                        }
                    }
                }
                td { (tx_output.index) }
                td {
                    @if is_token {
                        img src="/assets/slp-logo.png" {}
                    }
                }
                td {
                    .destination.hex {
                        @match &destination {
                            Destination::Address(address) => {a href={"/address/" (address.cash_addr())} {
                                (address.cash_addr())
                            }},
                            Destination::Nulldata(_ops) => "OP_RETURN data",
                            Destination::Unknown(_bytes) => "Unknown",
                        }
                    }
                }
                td {
                    .amount.hex {
                        @match (&tx_output.slp_token, token_meta) {
                            (Some(slp), Some(token)) if slp.amount > 0 => {
                                (render_amount(slp.amount, slp.decimals))
                                " "
                                (String::from_utf8_lossy(&token.token_ticker))
                                div {
                                    small {
                                        (render_sats(tx_output.value, true))
                                        " ABC"
                                    }
                                }
                            }
                            (Some(slp), Some(_)) if slp.is_mint_baton => {
                                .ui.green.horizontal.label { "Mint baton" }
                            }
                            _ => {
                                (render_sats(tx_output.value, true))
                                " ABC"
                            }
                        }
                    }
                }
                td.toggle {
                    i.icon.chevron.circle.down
                        id={"output-details-toggle-" (tx_output.index)}
                        onclick={(format!("toggleDetails('output', {0})", tx_output.index))} {}
                }
            }
            tr id={"output-details-" (tx_output.index)} style="display: none;" {
                td colspan="1" {}
                td colspan="5" {
                    p {
                        strong { "Output script hex" }
                        .hex { (hex::encode(&tx_output.pubkey_script)) }
                    }
                    p {
                        strong { "Output script decoded" }
                        .hex { (output_script) }
                    }
                }
            }
        }
    }

    pub fn render_input(
        &self,
        tx_input: &bchrpc::transaction::Input,
        token_meta: &Option<TokenMeta>,
    ) -> Markup {
        let is_token = tx_input.slp_token.as_ref().map(|slp| slp.amount > 0 || slp.is_mint_baton).unwrap_or(false);
        let destination = destination_from_script(
            if is_token { self.tokens_addr_prefix } else { self.satoshi_addr_prefix },
            &tx_input.previous_script,
        );
        let input_script = Script::deser_ops(tx_input.signature_script.as_slice().into())
            .map(|script| script.to_string())
            .unwrap_or("invalid script".to_string());
        html! {
            tr {
                td {
                    a href={"/tx/" (to_le_hex(&tx_input.outpoint.as_ref().expect("No outpoint").hash))} {
                        img src={"/assets/input.svg"} {}
                    }
                }
                td {
                    (tx_input.index)
                }
                td {
                    @if is_token {
                        img src="/assets/slp-logo.png" {}
                    }
                }
                td {
                    .destination.hex {
                        @match &destination {
                            Destination::Address(address) => {a href={"/address/" (address.cash_addr())} {
                                (address.cash_addr())
                            }},
                            Destination::Unknown(_bytes) => "Unknown",
                            Destination::Nulldata(_ops) => "Unreachable",
                        }
                    }
                }
                td {
                    .amount.hex {
                        @match (&tx_input.slp_token, token_meta) {
                            (Some(slp), Some(token)) if slp.amount > 0 => {
                                (render_amount(slp.amount, slp.decimals))
                                " "
                                (String::from_utf8_lossy(&token.token_ticker))
                                div {
                                    small {
                                        (render_sats(tx_input.value, true))
                                        " ABC"
                                    }
                                }
                            }
                            (Some(slp), Some(_)) if slp.is_mint_baton => {
                                .ui.green.horizontal.label { "Mint baton" }
                            }
                            _ => {
                                (render_sats(tx_input.value, true))
                                " ABC"
                            }
                        }
                    }
                }
                td.toggle {
                    i.icon.chevron.circle.down
                        id={"input-details-toggle-" (tx_input.index)}
                        onclick={(format!("toggleDetails('input', {0})", tx_input.index))} {}
                }
            }
            tr id={"input-details-" (tx_input.index)} style="display: none;" {
                td colspan="1" {}
                td colspan="5" {
                    p {
                        strong { "Input script hex" }
                        .hex { (hex::encode(&tx_input.signature_script)) }
                    }
                    p {
                        strong { "Input script decoded" }
                        .hex { (input_script) }
                    }
                }
            }
        }
    }
}

impl Server {
    pub async fn address(&self, address: &str) -> Result<impl Reply> {
        let address = Address::from_cash_addr(address)?;
        let address_txs = self.bchd.address(&address).await?;
        let json_txs = self.json_txs(address_txs.txs.iter().map(|addr_tx| {
            (addr_tx.tx.hash.as_slice(), addr_tx.timestamp, addr_tx.block_height, &addr_tx.tx_meta)
        })).await?;
        let balance = self.bchd.address_balance(&address).await?;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct JsonUtxo {
            tx_hash: String,
            out_idx: u32,
            sats_amount: i64,
            token_amount: u64,
            is_coinbase: bool,
            block_height: i32,
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct JsonBalance {
            token_idx: Option<usize>,
            sats_amount: i64,
            token_amount: u64,
            utxos: Vec<JsonUtxo>,
        }
        let AddressBalance { balances, utxos } = balance;
        let mut json_balances = utxos.into_iter().map(|(token_id, mut utxos)| {
            let (sats_amount, token_amount) = balances[&token_id];
            utxos.sort_by_key(|utxo| -utxo.block_height);
            (
                utxos[0].block_height,
                JsonBalance {
                    token_idx: token_id.and_then(|token_id| json_txs.token_indices.get(&token_id)).copied(),
                    sats_amount,
                    token_amount,
                    utxos: utxos.into_iter().map(|utxo| JsonUtxo {
                        tx_hash: to_le_hex(&utxo.tx_hash),
                        out_idx: utxo.out_idx,
                        sats_amount: utxo.sats_amount,
                        token_amount: utxo.token_amount,
                        is_coinbase: utxo.is_coinbase,
                        block_height: utxo.block_height,
                    }).collect(),
                }
            )
        }).collect::<Vec<_>>();
        json_balances.sort_by_key(|(block_height, balance)| {
            if balance.token_idx.is_none() {
                i32::MIN
            } else {
                -block_height
            }
        });
        let json_balances = json_balances.into_iter().map(|(_, balance)| balance).collect::<Vec<_>>();

        let encoded_txs = serde_json::to_string(&json_txs.txs)?;
        let encoded_tokens = serde_json::to_string(&json_txs.tokens)?;
        let encoded_balances = serde_json::to_string(&json_balances)?;

        let markup = html! {
            (DOCTYPE)
            head {
                title { "be.cash Block Explorer" }
                (self.head_common())
            }
            body {
                (self.toolbar())

                (PreEscaped(format!(
                    r#"<script type="text/javascript">
                        window.addrTxData = [];
                        window.addrBalances = [];
                        {{
                            var txs = JSON.parse('{encoded_txs}');
                            var tokens = JSON.parse('{encoded_tokens}');
                            var startIdx = window.addrTxData.length;
                            window.addrTxData.length += txs.length;
                            for (var i = 0; i < txs.length; ++i) {{
                                var tx = txs[i];
                                tx.token = tx.tokenIdx === null ? null : tokens[tx.tokenIdx];
                                window.addrTxData[startIdx + i] = tx;
                            }}
                            var balances = JSON.parse('{encoded_balances}');
                            window.addrBalances.length = balances.length;
                            for (var i = 0; i < balances.length; ++i) {{
                                var balance = balances[i];
                                balance.token = balance.tokenIdx === null ? null : tokens[balance.tokenIdx];
                                window.addrBalances[i] = balance;
                            }}
                        }}
                    </script>"#,
                    encoded_txs = encoded_txs,
                    encoded_tokens = encoded_tokens,
                    encoded_balances = encoded_balances,
                )))

                .ui.container {
                    table.ui.table {
                        @for balance in json_balances.iter() {
                            @let token = balance.token_idx.and_then(|token_idx| json_txs.tokens.get(token_idx));
                            @match token {
                                None => {
                                    tr {
                                        td colspan="2" {
                                            h1 {
                                                (render_sats(balance.sats_amount, true)) " ABC"
                                            }
                                        }
                                    }
                                },
                                Some(token) => {
                                    tr {
                                        td {
                                            (render_amount(balance.token_amount, token.decimals))
                                        }
                                        td {
                                            (PreEscaped(&token.token_ticker))
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }
        };
        Ok(warp::reply::html(markup.into_string()))
    }
}

impl Server {
    fn head_common(&self) -> Markup {
        html! {
            meta charset="utf-8";
            script
                src="https://code.jquery.com/jquery-3.1.1.min.js"
                integrity="sha256-hVVnYaiADRTO2PzUGmuLJr8BLUSjGIZsDYGmIJLv2b8="
                crossorigin="anonymous" {}
            script type="text/javascript" src="/code/semantic-ui/semantic.js?v=0" {}
            script type="text/javascript" src="/code/webix/webix.js?v=8.1.0" {}
            script type="text/javascript" src="/code/moment.min.js?v=0" {}
            script type="text/javascript" src="/code/common.js" {}
            link rel="stylesheet" href="/code/webix/webix.css";
            link rel="stylesheet" href="/code/semantic-ui/semantic.css";
            link rel="stylesheet" href="/code/styles/index.css";
            link rel="preconnect" href="https://fonts.gstatic.com";
            link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Ubuntu+Mono&display=swap";
        }
    }

    fn toolbar(&self) -> Markup {
        html! {
            .ui.main.menu {
                .header.item {
                    img.logo src="/assets/logo.png" {}
                    "be.cash Explorer"
                }
                a.item href="/blocks" { "Blocks" }
                .item {
                    #search-box.ui.transparent.icon.input {
                        input type="text" placeholder="Search blocks, transactions, adddresses, tokens..." {}
                        i.search.link.icon {}
                    }
                }
                .ui.right.floated.dropdown.item href="#" {
                    "Bitcoin ABC"
                    i.dropdown.icon {}
                    .menu {
                        .item { "Bitcoin ABC" }
                    }
                }
            }
            script { (PreEscaped(r#"
                $('.main.menu  .ui.dropdown').dropdown({
                    on: 'hover'
                });
            "#)) }
        }
    }

    fn render_timestamp(&self, timestamp: i64) -> Markup {
        html! {
            (PreEscaped(format!(
                r#"<script type="text/javascript">
                    document.write(moment({timestamp}).format('L LTS'));
                    document.write(' <small>(UTC' + tzOffset + ')</small>');
                </script>"#,
                timestamp=timestamp * 1000,
            )))
        }
    }
}
