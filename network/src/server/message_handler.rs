use crate::{
    message::{types::*, Channel, Message},
    process_transaction_internal,
    propagate_block,
    Pings,
    Server,
    SyncState,
};
use snarkos_consensus::miner::Entry;
use snarkos_errors::network::ServerError;
use snarkos_objects::{Block as BlockStruct, BlockHeaderHash, Transaction as TransactionStruct};

use chrono::Utc;
use std::sync::Arc;

impl Server {
    /// handle an incoming message
    pub(in crate::server) async fn message_handler(&mut self) -> Result<(), ServerError> {
        while let Some((tx, name, bytes, mut channel)) = self.receiver.recv().await {
            if name == Block::name() {
                self.receive_block_message(Block::deserialize(bytes)?, channel.clone(), true)
                    .await?;
            } else if name == GetBlock::name() {
                self.receive_get_block(GetBlock::deserialize(bytes)?, channel.clone())
                    .await?;
            } else if name == GetMemoryPool::name() {
                self.receive_get_memory_pool(GetMemoryPool::deserialize(bytes)?, channel.clone())
                    .await?;
            } else if name == GetPeers::name() {
                self.receive_get_peers(GetPeers::deserialize(bytes)?, channel.clone())
                    .await?;
            } else if name == GetSync::name() {
                self.receive_get_sync(GetSync::deserialize(bytes)?, channel.clone())
                    .await?;
            } else if name == MemoryPool::name() {
                self.receive_memory_pool(MemoryPool::deserialize(bytes)?).await?;
            } else if name == Peers::name() {
                self.receive_peers(Peers::deserialize(bytes)?, channel.clone()).await?;
            } else if name == Ping::name() {
                self.receive_ping(Ping::deserialize(bytes)?, channel.clone()).await?;
            } else if name == Pong::name() {
                self.receive_pong(Pong::deserialize(bytes)?, channel.clone()).await?;
            } else if name == Sync::name() {
                self.receive_sync(Sync::deserialize(bytes)?).await?;
            } else if name == SyncBlock::name() {
                self.receive_block_message(Block::deserialize(bytes)?, channel.clone(), false)
                    .await?;
            } else if name == Transaction::name() {
                self.receive_transaction(Transaction::deserialize(bytes)?, channel.clone())
                    .await?;
            } else if name == Version::name() {
                channel = self
                    .receive_version(Version::deserialize(bytes)?, channel.clone())
                    .await?;
            } else if name == Verack::name() {
                self.receive_verack(Verack::deserialize(bytes)?, channel.clone())
                    .await?;
            } else {
                info!("Name not recognized {:?}", name.to_string());
            }
            tx.send(channel).expect("error resetting message handler");
        }
        Ok(())
    }

    /// A peer has sent us a new block to process
    async fn receive_block_message(
        &mut self,
        message: Block,
        channel: Arc<Channel>,
        propagate: bool,
    ) -> Result<(), ServerError> {
        let block = BlockStruct::deserialize(&message.data)?;

        // verify the block and insert it into the storage
        if !self.storage.is_exist(&block.header.get_hash()) {
            let mut memory_pool = self.memory_pool_lock.lock().await;
            let inserted = self
                .consensus
                .receive_block(&self.storage, &mut memory_pool, &block)
                .is_ok();
            drop(memory_pool);

            let mut sync_handler = self.sync_handler_lock.lock().await;

            if inserted && propagate {
                // This is a new block, send it to our peers

                propagate_block(self.context.clone(), message.data, channel.address).await?;
            } else if !propagate && sync_handler.sync_state != SyncState::Idle {
                // We are syncing with another node, ask for the next block

                if let Some(channel) = self.context.connections.read().await.get(&sync_handler.sync_node) {
                    sync_handler.increment(channel, Arc::clone(&self.storage)).await?;
                }
            }
        }

        Ok(())
    }

    /// A peer has requested a block
    async fn receive_get_block(&mut self, message: GetBlock, channel: Arc<Channel>) -> Result<(), ServerError> {
        if let Ok(block) = self.storage.get_block(message.block_hash) {
            channel.write(&SyncBlock::new(block.serialize()?)).await?;
        }

        Ok(())
    }

    /// A peer has requested our memory pool transactions
    async fn receive_get_memory_pool(
        &mut self,
        _message: GetMemoryPool,
        channel: Arc<Channel>,
    ) -> Result<(), ServerError> {
        let memory_pool = self.memory_pool_lock.lock().await;

        let mut transactions = vec![];

        for (_tx_id, entry) in &memory_pool.transactions {
            if let Ok(transaction_bytes) = entry.transaction.serialize() {
                transactions.push(transaction_bytes);
            }
        }
        drop(memory_pool);

        if !transactions.is_empty() {
            channel.write(&MemoryPool::new(transactions)).await?;
        }

        Ok(())
    }

    /// A peer has sent us their memory pool transactions
    async fn receive_memory_pool(&mut self, message: MemoryPool) -> Result<(), ServerError> {
        let mut memory_pool = self.memory_pool_lock.lock().await;

        for transaction_bytes in message.transactions {
            let entry = Entry {
                size: transaction_bytes.len(),
                transaction: TransactionStruct::deserialize(&transaction_bytes)?,
            };

            if let Ok(inserted) = memory_pool.insert(&self.storage, entry) {
                if let Some(txid) = inserted {
                    info!("Transaction added to memory pool with txid: {:?}", hex::encode(txid));
                }
            }
        }

        Ok(())
    }

    /// A node has requested our list of peer addresses
    /// send an Address message with our current peer list
    async fn receive_get_peers(&mut self, _message: GetPeers, channel: Arc<Channel>) -> Result<(), ServerError> {
        channel
            .write(&Peers::new(self.context.peer_book.read().await.peers.addresses.clone()))
            .await?;

        Ok(())
    }

    /// A miner has sent their list of peer addresses
    /// send a Version message to each peer in the list
    /// this is going to be a lot of awaits in a loop...
    /// can look at futures crate to handle multiple futures
    async fn receive_peers(&mut self, message: Peers, channel: Arc<Channel>) -> Result<(), ServerError> {
        let mut peer_book = self.context.peer_book.write().await;
        for (addr, time) in message.addresses.iter() {
            if &self.context.local_addr == addr {
                continue;
            } else if peer_book.peer_contains(addr) {
                peer_book.peers.update(addr.clone(), time.clone());
            } else if peer_book.disconnected_contains(addr) {
                peer_book.disconnected.remove(addr);
                peer_book.gossiped.update(addr.clone(), time.clone());
            } else {
                peer_book.gossiped.update(addr.clone(), time.clone());
            }
        }

        peer_book.peers.update(channel.address, Utc::now());

        Ok(())
    }

    async fn receive_ping(&mut self, message: Ping, channel: Arc<Channel>) -> Result<(), ServerError> {
        Pings::send_pong(message, channel).await?;

        Ok(())
    }

    async fn receive_pong(&mut self, message: Pong, channel: Arc<Channel>) -> Result<(), ServerError> {
        match self
            .context
            .pings
            .write()
            .await
            .accept_pong(channel.address, message)
            .await
        {
            Ok(()) => {
                self.context
                    .peer_book
                    .write()
                    .await
                    .peers
                    .update(channel.address, Utc::now());
            }
            Err(error) => info!(
                "Invalid Pong message from: {:?}, Full error: {:?}",
                channel.address, error
            ),
        }

        Ok(())
    }

    /// A peer has requested our chain state to sync with
    async fn receive_get_sync(&mut self, message: GetSync, channel: Arc<Channel>) -> Result<(), ServerError> {
        let latest_shared_hash = self.storage.get_latest_shared_hash(message.block_locator_hashes)?;
        let current_height = self.storage.get_latest_block_height();

        if let Ok(height) = self.storage.get_block_num(&latest_shared_hash) {
            if height < current_height {
                let mut max_height = current_height;

                // if the requester is behind more than 100 blocks
                if height + 100 < current_height {
                    // send the max 100 blocks
                    max_height = height + 100;
                }

                let mut block_hashes: Vec<BlockHeaderHash> = vec![];

                for block_num in height + 1..=max_height {
                    block_hashes.push(self.storage.get_block_hash(block_num)?);
                }

                // send serialized blocks to requester
                channel.write(&Sync::new(block_hashes)).await?;
            }
        }
        //        }
        Ok(())
    }

    /// A peer has sent us their chain state
    async fn receive_sync(&mut self, message: Sync) -> Result<(), ServerError> {
        let mut sync_handler = self.sync_handler_lock.lock().await;

        for block_hash in message.block_hashes {
            if !sync_handler.block_headers.contains(&block_hash) {
                sync_handler.block_headers.push(block_hash.clone());
            }
            sync_handler.update_syncing(self.storage.get_latest_block_height());
        }

        if let Some(channel) = self.context.connections.read().await.get(&sync_handler.sync_node) {
            sync_handler.increment(channel, Arc::clone(&self.storage)).await?;
        }

        Ok(())
    }

    /// A peer has sent us a transaction
    async fn receive_transaction(&mut self, message: Transaction, channel: Arc<Channel>) -> Result<(), ServerError> {
        process_transaction_internal(
            self.context.clone(),
            self.storage.clone(),
            self.memory_pool_lock.clone(),
            message.bytes,
            channel.address,
        )
        .await?;

        Ok(())
    }

    /// A new peer has acknowledged our Version message
    /// add them to our peer book
    async fn receive_verack(&mut self, message: Verack, channel: Arc<Channel>) -> Result<(), ServerError> {
        match self
            .context
            .handshakes
            .write()
            .await
            .accept_response(channel.address, message)
            .await
        {
            Ok(()) => {
                let peer_book = &mut self.context.peer_book.write().await;

                if &self.context.local_addr != &channel.address {
                    peer_book.disconnected.remove(&channel.address);
                    peer_book.gossiped.remove(&channel.address);
                    peer_book.peers.update(channel.address, Utc::now());
                }

                // get new peer's peers
                channel.write(&GetPeers).await?;
            }
            Err(error) => {
                info!(
                    "Invalid Verack message from: {:?} Full error: {:?}",
                    channel.address,
                    ServerError::HandshakeError(error)
                );
            }
        }
        Ok(())
    }

    /// A miner is trying to connect with us
    /// check if sending node is a new peer
    async fn receive_version(&mut self, message: Version, channel: Arc<Channel>) -> Result<Arc<Channel>, ServerError> {
        let peer_address = message.address_sender;
        let peer_book = &mut self.context.peer_book.read().await;

        if peer_book.peers.addresses.len() < self.context.max_peers as usize && self.context.local_addr != peer_address
        {
            self.context
                .handshakes
                .write()
                .await
                .receive_request(message.clone(), peer_address)
                .await?;

            // if our peer has a longer chain, send a sync message
            if message.height > self.storage.get_latest_block_height() {
                let mut sync_handler = self.sync_handler_lock.lock().await;
                sync_handler.sync_node = peer_address;

                if let Ok(block_locator_hashes) = self.storage.get_block_locator_hashes() {
                    channel.write(&GetSync::new(block_locator_hashes)).await?;
                }
            }
        }
        Ok(channel)
    }
}