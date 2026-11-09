//! Application ownership. Host users, the local socket and Discord moderators remain trusted.
use super::*;
#[derive(Debug)]
pub struct Denied;
impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("permission denied: private conversation unavailable")
    }
}
impl std::error::Error for Denied {}

impl State {
    pub(super) fn claim_in(
        tx: &rusqlite::Transaction<'_>,
        person: &str,
        session: &str,
    ) -> Result<()> {
        let other:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM session_people WHERE session=?1 AND person<>?2 UNION ALL SELECT 1 FROM people WHERE session=?1 AND id<>?2 UNION ALL SELECT 1 FROM conversations WHERE session=?1 AND person<>?2)",params![session,person],|r|r.get(0))?;
        if other {
            return Err(anyhow::anyhow!(Denied));
        }
        tx.execute(
            "INSERT OR IGNORE INTO session_people VALUES(?1,?2)",
            params![session, person],
        )?;
        Ok(())
    }
    pub fn claim_session(&self, person: &str, session: &str) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        Self::claim_in(&tx, person, session)?;
        tx.commit()?;
        Ok(())
    }
    pub fn session_owner(&self, session: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT person FROM session_people WHERE session=?1",
                [session],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn ambiguous_session(&self, session: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row("SELECT COUNT(DISTINCT person)>1 FROM (SELECT id AS person FROM people WHERE session=?1 UNION ALL SELECT person FROM conversations WHERE session=?1 UNION ALL SELECT person FROM inbox WHERE session=?1)",[session],|r|r.get(0))?)
    }
    pub fn assert_owner(&self, person: &str, session: &str) -> Result<()> {
        if self.session_owner(session)?.as_deref() != Some(person) {
            return Err(anyhow::anyhow!(Denied));
        }
        Ok(())
    }
    pub fn selectable(&self, to: &Address, session: &str) -> Result<bool> {
        if self.session_owner(session)?.as_deref() != Some(to.person.as_str()) {
            return Ok(false);
        }
        let place: Option<(String, String)> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT channel,destination FROM session_places WHERE session=?1",
                [session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match place {
            Some((channel, destination)) => channel == to.channel && destination == to.destination,
            None => self.conversation(to)?.is_none(),
        })
    }
    pub(super) fn place_in(
        tx: &rusqlite::Transaction<'_>,
        to: &Address,
        session: &str,
    ) -> Result<()> {
        let place: Option<(String, String)> = tx
            .query_row(
                "SELECT channel,destination FROM session_places WHERE session=?1",
                [session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if place.is_some_and(|(c, d)| c != to.channel || d != to.destination) {
            return Err(anyhow::anyhow!(Denied));
        }
        tx.execute(
            "INSERT OR IGNORE INTO session_places VALUES(?1,?2,?3)",
            params![session, to.channel, to.destination],
        )?;
        Ok(())
    }
    pub(super) fn import_ownership(&self) -> Result<()> {
        self.db.lock().unwrap().execute_batch(
            "INSERT OR IGNORE INTO session_people(session,person)
            SELECT session,MIN(person) FROM (
                SELECT session,id AS person FROM people WHERE session IS NOT NULL
                UNION ALL SELECT session,person FROM conversations
                UNION ALL SELECT session,person FROM inbox WHERE session IS NOT NULL
            ) GROUP BY session HAVING COUNT(DISTINCT person)=1;",
        )?;
        Ok(())
    }
}

impl State {
    pub fn assert_destination(&self,to:&Address)->Result<()> {
        if to.channel!="discord" {return Ok(());}
        if let Some(route)=self.thread_owner(&to.destination)? {
            if route["person"]==to.person && route["status"]=="active" {return Ok(());}
        }else if self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM identities WHERE channel='discord' AND person=?1 AND destination=?2)",params![to.person,to.destination],|r|r.get::<_,bool>(0))? {return Ok(());}
        Err(anyhow::anyhow!(Denied))
    }
}
