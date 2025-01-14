use std::{cmp, collections::HashMap, time::Duration};

use anyhow::{anyhow, Result};
use futures::FutureExt;
use penumbra_keys::Address;
use penumbra_num::Amount;
use tokio::sync::mpsc::{self};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};

use crate::{participant::Participant, phase::Phase, storage::Storage};

struct ContributionHandler {
    storage: Storage,
    start_contribution_rx: mpsc::Receiver<(Address, Participant)>,
    done_contribution_tx: mpsc::Sender<()>,
}

impl ContributionHandler {
    pub fn new(
        storage: Storage,
    ) -> (
        Self,
        mpsc::Sender<(Address, Participant)>,
        mpsc::Receiver<()>,
    ) {
        let (start_contribution_tx, start_contribution_rx) = mpsc::channel(1);
        let (done_contribution_tx, done_contribution_rx) = mpsc::channel(1);
        (
            Self {
                storage,
                start_contribution_rx,
                done_contribution_tx,
            },
            start_contribution_tx,
            done_contribution_rx,
        )
    }

    #[tracing::instrument(skip(self))]
    pub async fn run<P: Phase>(mut self) -> Result<()> {
        loop {
            tracing::debug!("start of contribution handler loop");
            let (who, participant) = match self.start_contribution_rx.recv().await {
                None => {
                    tracing::debug!("start channel closed.");
                    return Ok(());
                }
                Some((w, p)) => (w, p),
            };
            tracing::debug!(?who, "waiting for contribution");
            self.contribute::<P>(who, participant).await?;
            self.done_contribution_tx.send(()).await?;
        }
    }

    #[tracing::instrument(skip(self, participant))]
    async fn contribute<P: Phase>(
        &mut self,
        contributor: Address,
        participant: Participant,
    ) -> Result<()> {
        match tokio::time::timeout(
            Duration::from_secs(P::CONTRIBUTION_TIME_SECS),
            self.contribute_inner::<P>(contributor, participant),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Err(_) => {
                tracing::info!("STRIKE (timeout)");
                self.storage.strike(&contributor).await?;
                Ok(())
            }
            Ok(Err(e)) => Err(e),
        }
    }

    #[tracing::instrument(skip_all)]
    async fn contribute_inner<P: Phase>(
        &mut self,
        contributor: Address,
        mut participant: Participant,
    ) -> Result<()> {
        let parent = P::current_crs(&self.storage)
            .await?
            .expect("the phase should've been initialized by now");
        let maybe = participant.contribute::<P>(&parent).await?;
        if let Some(unvalidated) = maybe {
            tracing::debug!("validating contribution");
            let root = P::fetch_root(&self.storage).await?;
            let maybe_contribution = tokio::task::spawn_blocking(move || {
                if let Some(contribution) = P::validate(&root, unvalidated) {
                    if P::is_linked_to(&contribution, &parent) {
                        return Some(contribution);
                    }
                }
                None
            })
            .await?;
            if let Some(contribution) = maybe_contribution {
                P::commit_contribution(&self.storage, contributor, contribution).await?;
                participant
                    .confirm(self.storage.current_slot(P::MARKER).await?)
                    .await?;
                return Ok(());
            }
        }
        tracing::info!("STRIKE (invalid or partial contribution)");
        self.storage.strike(&contributor).await?;
        return Ok(());
    }
}

struct ParticipantQueue {
    participants: HashMap<Address, (Participant, Amount)>,
}

impl ParticipantQueue {
    fn new() -> Self {
        Self {
            participants: HashMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.participants.len()
    }

    fn bid(&self, address: &Address) -> Option<Amount> {
        self.participants.get(address).map(|(_, bid)| *bid)
    }

    fn add(&mut self, participant: Participant, bid: Amount) {
        let address = participant.address();
        tracing::info!(?address, "has been added as a participant");
        self.participants.insert(address, (participant, bid));
    }

    fn prune(&mut self) {
        self.participants
            .retain(|_, (connection, _)| connection.is_live());
    }

    fn score(&self) -> Vec<Address> {
        let mut out: Vec<Address> = self.participants.keys().cloned().collect();
        out.sort_by_cached_key(|addr| cmp::Reverse(self.participants[addr].1));
        out
    }

    fn remove(&mut self, address: &Address) -> Option<(Participant, Amount)> {
        self.participants.remove(address)
    }

    /// Inform participants of their position in the queue.
    ///
    /// If filter is not None, only one participant will be informed.
    async fn inform(
        &mut self,
        ranked: &[Address],
        contributor_bid: Amount,
        filter: Option<Address>,
    ) {
        for (i, address) in ranked.iter().enumerate() {
            match filter {
                Some(filter) if filter != *address => continue,
                _ => {}
            }
            let (connection, bid) = self
                .participants
                .get(address)
                .expect("Ranked participants are chosen from the set of connections");
            if let Err(e) =
                connection.try_notify(i as u32, ranked.len() as u32, contributor_bid, *bid)
            {
                tracing::info!(?e, ?address, "pruning connection that we failed to notify");
                self.participants.remove(address);
            };
        }
    }
}

pub struct Coordinator {
    storage: Storage,
    participants: ParticipantQueue,
    new_participant_rx: mpsc::Receiver<(Participant, Amount)>,
}

impl Coordinator {
    pub fn new(storage: Storage) -> (Self, mpsc::Sender<(Participant, Amount)>) {
        let (new_participant_tx, new_participant_rx) = mpsc::channel(9001);
        (
            Self {
                storage,
                participants: ParticipantQueue::new(),
                new_participant_rx,
            },
            new_participant_tx,
        )
    }

    pub async fn run<P: Phase + 'static>(mut self) -> Result<()> {
        enum Event {
            NewParticipant(Participant, Amount),
            ContributionDone,
            ContributionHandlerFinished(Result<()>),
        }

        let (contribution_handler, start_contribution_tx, done_contribution_rx) =
            ContributionHandler::new(self.storage);
        let wait_for_contribution_handler = tokio::spawn(contribution_handler.run::<P>());
        // Merge the events from both being notified of new participants, and of completed
        // contributions.
        let mut stream = ReceiverStream::new(self.new_participant_rx)
            .map(|(participant, bid)| Event::NewParticipant(participant, bid))
            .merge(ReceiverStream::new(done_contribution_rx).map(|_| Event::ContributionDone))
            .merge(wait_for_contribution_handler.into_stream().map(|x| {
                Event::ContributionHandlerFinished(x.unwrap_or(Err(anyhow!(
                    "failed to join on contribution handler handle"
                ))))
            }));

        // We start by needing a contribution.
        let mut want_contribution = true;
        loop {
            tracing::debug!(
                participant_count = self.participants.len(),
                "top of coordinator loop"
            );
            // 1. Wait for a new event
            let maybe_new_address = match stream.next().await {
                None => anyhow::bail!("coordinator event stream closed unexpectedly."),
                Some(Event::NewParticipant(participant, bid)) => {
                    let addr = participant.address();
                    self.participants.add(participant, bid);
                    Some(addr)
                }
                Some(Event::ContributionDone) => {
                    // We always want a new contribution now.
                    want_contribution = true;
                    None
                }
                Some(Event::ContributionHandlerFinished(why)) => {
                    return Err(why
                        .err()
                        .unwrap_or(anyhow!("contribution handler finished with no reason")));
                }
            };
            // 2. Score connections
            self.participants.prune();
            let ranked = self.participants.score();
            // In theory ranked could've become empty for some reason in the meantime
            if ranked.is_empty() {
                continue;
            }
            // 3. Update people on their status in the queue.
            //
            // The intention of this loop is that when someone joins, they get a message with their
            // position in the queue, but other people don't receive updates, to avoid
            // amplification attacks when repeatedly connecting and disconnecting. However, we want
            // to inform everyone when a new "round" starts.
            let contributor = ranked[0];
            let contributor_bid = self
                .participants
                .bid(&contributor)
                .expect("contributor should be in participant queue");
            self.participants
                .inform(&ranked, contributor_bid, maybe_new_address)
                .await;
            // 4. If we want a new contribution, get that process going.
            if want_contribution {
                // 5. Remove from pool regardless of what will happen
                let (participant, _) = self
                    .participants
                    .remove(&contributor)
                    .expect("the selected contributor exists");
                start_contribution_tx
                    .send((contributor, participant))
                    .await
                    .map_err(|_| anyhow!("failed to send start contribution message to handler"))?;
                // 6. We no longer want to make a new contribution until this one finishes.
                want_contribution = false;
            }
        }
    }
}
