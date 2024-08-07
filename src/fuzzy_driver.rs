use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use arbitrary::Unstructured;
use rusqlite::Connection;
use scc::HashSet as ConcurrentHashSet;
use scupt_net::message_receiver_async::ReceiverAsync;
use scupt_net::message_sender_async::SenderAsync;
use scupt_net::notifier::Notifier;
use scupt_net::opt_send::OptSend;
use scupt_net::task::spawn_local_task;
use scupt_util::error_type::ET;
use scupt_util::message::Message;
use scupt_util::node_id::NID;
use scupt_util::res::Res;
use scupt_util::res_of::res_sqlite;
use scupt_util::serde_json_string::SerdeJsonString;
use tokio::time::sleep;

use crate::fuzzy_command::FuzzyCommand;
use crate::fuzzy_event::FuzzyEvent;
use crate::event_gen::EventGen;
use crate::fuzzy_setting::FuzzySetting;

#[derive(Clone)]
pub struct FuzzyDriver {
    path_store: String,
    notifier: Notifier,
    inner: Arc<FuzzyInner>,
    event_gen:EventGen,
}

struct FuzzyInner {
    dis_connect: ConcurrentHashSet<(NID, NID)>,
    atomic_sequence: AtomicU64,
    sender: Arc<dyn SenderAsync<SerdeJsonString>>,
    path: String,
}

impl FuzzyDriver {
    pub fn new(
        path: String,
        notifier: Notifier,
        node_set: HashSet<NID>,
        setting:FuzzySetting,
        sender: Arc<dyn SenderAsync<SerdeJsonString>> ) -> Self {
        Self {
            path_store: path.clone(),
            notifier,
            inner: Arc::new(FuzzyInner {
                dis_connect: Default::default(),
                atomic_sequence: AtomicU64::new(0),
                sender,
                path,
            }),
            event_gen: EventGen::new(node_set.iter().cloned().collect(), setting),
        }
    }

    pub fn create_db(&self) -> Res<()> {
        let mut conn = Connection::open(self.path_store.clone()).unwrap();
        let trans = res_sqlite(conn.transaction())?;
        let _r = trans.execute(
            r#"create table action (
                    id interger primary key,
                    event text not null
                );"#, ());
        res_sqlite(_r)?;
        let _r = trans.execute(
            r#"create table delivery (
                    id interger primary key,
                    action_id integer not null
                );"#, ());
        res_sqlite(_r)?;
        trans.commit().unwrap();
        Ok(())
    }


    pub async fn message_loop(
        &self,
        receiver: Arc<dyn ReceiverAsync<FuzzyCommand>>,
        data: Vec<u8>,
    ) -> Res<()> {
        let mut u = Unstructured::new(data.as_slice());
        loop {
            let msg = receiver.receive().await?;
            self.incoming_command(msg.payload(), &mut u).await?;
        }
    }

    pub async fn incoming_command(&self, command: FuzzyCommand, unstructured: &mut Unstructured<'_>) -> Res<()> {
        match command {
            FuzzyCommand::MessageReq(m) => {
                let mut vec = vec![];
                let cont = self.event_gen.fuzz_message(&m, unstructured, &mut vec);

                for event in vec {
                    let id = self.inner.gen_id();
                    self.fuzzy_event_for_message(id, event).await?;
                }
                if !cont {
                    return Err(ET::EOF);
                }
            }
        }
        Ok(())
    }

    fn store_event_message(&self, id: u64, event: FuzzyEvent) {
        let mut conn = Connection::open(self.path_store.clone()).unwrap();
        let transaction = conn.transaction().unwrap();
        let event_s = serde_json::to_string_pretty(&event).unwrap();
        let _ = transaction.execute("\
                            insert into action(id,  event) \
                            values(?1, ?2)", (&id, &event_s)).unwrap();
        transaction.commit().unwrap();
    }

    async fn fuzzy_event_for_message(&self, id: u64, event: FuzzyEvent) -> Res<()> {
        self.store_event_message(id, event.clone());
        self.schedule_fuzzy_event(id, event).await?;
        Ok(())
    }

    async fn schedule_fuzzy_event(&self, id: u64, event: FuzzyEvent) -> Res<()> {
        let inner = self.inner.clone();
        let _ = spawn_local_task(self.notifier.clone(), "", async move {
            inner.schedule(id, event).await?;
            Ok::<(), ET>(())
        })?;
        Ok(())
    }
}

impl FuzzyInner {
    async fn schedule(&self, id: u64, event: FuzzyEvent) -> Res<()> {
        match event {
            FuzzyEvent::Delay(ms, message) => {
                if ms > 0 {
                    sleep(Duration::from_millis(ms)).await;
                }
                self.send(id, message).await?;
            }
            FuzzyEvent::Duplicate(vec, message) => {
                for ms in vec {
                    sleep(Duration::from_millis(ms)).await;
                    self.send(id, message.clone()).await?;
                }
            }
            FuzzyEvent::Lost => {}
            FuzzyEvent::Restart(ms, message) => {
                sleep(Duration::from_millis(ms)).await;
                self.send(id, message).await?;
            }
            FuzzyEvent::Crash(message) => {
                self.send(id, message).await?;
            }
            FuzzyEvent::PartitionStart(ids1, ids2) => {
                self.partition_start(ids1, ids2);
            }
            FuzzyEvent::PartitionRecovery(ms, ids1, ids2) => {
                sleep(Duration::from_millis(ms)).await;
                self.partition_end(ids1, ids2);
            }
        }
        Ok(())
    }

    async fn send(&self, id: u64, message: Message<String>) -> Res<()> {
        if !self.can_connect(message.source(), message.dest()) {
            return Ok(());
        }
        self.store_message_delivery(id);
        let m = message.map(|s| {
            SerdeJsonString::new(s)
        });
        let _ = self.sender.send(m, OptSend::default()).await?;
        Ok(())
    }

    fn gen_id(&self) -> u64 {
        let id = self.atomic_sequence.fetch_add(1, Ordering::SeqCst);
        return id;
    }

    fn store_message_delivery(&self, action_id: u64) {
        let mut conn = Connection::open(self.path.clone()).unwrap();
        let id = self.gen_id();
        let transaction = conn.transaction().unwrap();
        let _ = transaction.execute(
            r#"insert into delivery(id, action_id)
                   values(?1, ?2)"#, (&id, &action_id)).unwrap();

        transaction.commit().unwrap();
    }

    fn can_connect(&self, id1: NID, id2: NID) -> bool {
        !self.dis_connect.contains(&(id1, id2))
    }

    fn partition_end(&self, ids1: Vec<NID>, ids2: Vec<NID>) {
        for i in &ids1 {
            for j in &ids2 {
                let _ = self.dis_connect.remove(&(*i, *j));
            }
        }
    }

    fn partition_start(&self, ids1: Vec<NID>, ids2: Vec<NID>) {
        for i in &ids1 {
            for j in &ids2 {
                if *i != *j {
                    let _ = self.dis_connect.insert((*i, *j));
                }
            }
        }
    }
}
