// Copyright (C) 2015 Wire Swiss GmbH <support@wire.com>
// Based on libsignal-protocol-java by Open Whisper Systems
// https://github.com/WhisperSystems/libsignal-protocol-java.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use cbor::{self, Config, Decoder, Encoder};
use cbor::skip::Skip;
use hkdf::{Input, Info, Salt};
use internal::derived::{DerivedSecrets, CipherKey, MacKey};
use internal::keys;
use internal::keys::{IdentityKey, IdentityKeyPair, PreKeyBundle, PreKey, PreKeyId};
use internal::keys::{KeyPair, PublicKey};
use internal::message::{Counter, PreKeyMessage, Envelope, Message, CipherMessage, SessionTag};
use internal::types::{DecodeError, DecodeResult, EncodeResult, InternalError};
use std::borrow::Cow;
use std::cmp::{Ord, Ordering};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::io::{Cursor, Read, Write};
use std::usize;
use std::vec::Vec;

// Root key /////////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct RootKey {
    key: CipherKey
}

impl RootKey {
    pub fn from_cipher_key(k: CipherKey) -> RootKey {
        RootKey { key: k }
    }

    pub fn dh_ratchet(&self, ours: &KeyPair, theirs: &PublicKey) -> (RootKey, ChainKey) {
        let secret = ours.secret_key.shared_secret(theirs);
        let dsecs  = DerivedSecrets::kdf(Input(&secret), Salt(&self.key), Info(b"dh_ratchet"));
        (RootKey::from_cipher_key(dsecs.cipher_key), ChainKey::from_mac_key(dsecs.mac_key, Counter::zero()))
    }

    fn encode<W: Write>(&self, e: &mut Encoder<W>) -> EncodeResult<()> {
        try!(e.object(1));
        try!(e.u8(0)); self.key.encode(e)
    }

    fn decode<R: Read + Skip>(d: &mut Decoder<R>) -> DecodeResult<RootKey> {
        let n = try!(d.object());
        let mut key = None;
        for _ in 0 .. n {
            match try!(d.u8()) {
                0 => key = Some(try!(CipherKey::decode(d))),
                _ => try!(d.skip())
            }
        }
        Ok(RootKey { key: to_field!(key, "RootKey::key") })
    }
}

// Chain key /////////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct ChainKey {
    key: MacKey,
    idx: Counter
}

impl ChainKey {
    pub fn from_mac_key(k: MacKey, idx: Counter) -> ChainKey {
        ChainKey { key: k, idx: idx }
    }

    pub fn next(&self) -> ChainKey {
        ChainKey {
            key: MacKey::new(self.key.sign(b"1").into_bytes()),
            idx: self.idx.next()
        }
    }

    pub fn message_keys(&self) -> MessageKeys {
        let base  = self.key.sign(b"0");
        let dsecs = DerivedSecrets::kdf_without_salt(Input(&base), Info(b"hash_ratchet"));
        MessageKeys {
            cipher_key: dsecs.cipher_key,
            mac_key:    dsecs.mac_key,
            counter:    self.idx
        }
    }

    fn encode<W: Write>(&self, e: &mut Encoder<W>) -> EncodeResult<()> {
        try!(e.object(2));
        try!(e.u8(0)); try!(self.key.encode(e));
        try!(e.u8(1)); self.idx.encode(e)
    }

    fn decode<R: Read + Skip>(d: &mut Decoder<R>) -> DecodeResult<ChainKey> {
        let n = try!(d.object());
        let mut key = None;
        let mut idx = None;
        for _ in 0 .. n {
            match try!(d.u8()) {
                0 => key = Some(try!(MacKey::decode(d))),
                1 => idx = Some(try!(Counter::decode(d))),
                _ => try!(d.skip())
            }
        }
        Ok(ChainKey {
            key: to_field!(key, "ChainKey::key"),
            idx: to_field!(idx, "ChainKey::idx")
        })
    }
}

// Send Chain ///////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct SendChain {
    chain_key:   ChainKey,
    ratchet_key: KeyPair
}

impl SendChain {
    pub fn new(ck: ChainKey, rk: KeyPair) -> SendChain {
        SendChain { chain_key: ck, ratchet_key: rk }
    }

    fn encode<W: Write>(&self, e: &mut Encoder<W>) -> EncodeResult<()> {
        try!(e.object(2));
        try!(e.u8(0)); try!(self.chain_key.encode(e));
        try!(e.u8(1)); self.ratchet_key.encode(e)
    }

    fn decode<R: Read + Skip>(d: &mut Decoder<R>) -> DecodeResult<SendChain> {
        let n = try!(d.object());
        let mut chain_key   = None;
        let mut ratchet_key = None;
        for _ in 0 .. n {
            match try!(d.u8()) {
                0 => chain_key   = Some(try!(ChainKey::decode(d))),
                1 => ratchet_key = Some(try!(KeyPair::decode(d))),
                _ => try!(d.skip())
            }
        }
        Ok(SendChain {
            chain_key:   to_field!(chain_key, "SendChain::chain_key"),
            ratchet_key: to_field!(ratchet_key, "SendChain::ratchet_key")
        })
    }
}

// Receive Chain ////////////////////////////////////////////////////////////

const MAX_COUNTER_GAP: usize = 1000;

#[derive(Clone)]
pub struct RecvChain {
    chain_key:    ChainKey,
    ratchet_key:  PublicKey,
    message_keys: VecDeque<MessageKeys>
}

impl RecvChain {
    pub fn new(ck: ChainKey, rk: PublicKey) -> RecvChain {
        RecvChain {
            chain_key:    ck,
            ratchet_key:  rk,
            message_keys: VecDeque::new()
        }
    }

    fn try_message_keys<E>(&mut self, env: &Envelope, mesg: &CipherMessage) -> Result<Vec<u8>, DecryptError<E>> {
        let too_old = self.message_keys.get(0)
            .map(|k| k.counter > mesg.counter)
            .unwrap_or(false);

        if too_old {
            return Err(DecryptError::OutdatedMessage)
        }

        match self.message_keys.iter().position(|mk| mk.counter == mesg.counter) {
            Some(i) => {
                let mk = self.message_keys.remove(i).unwrap();
                if env.verify(&mk.mac_key) {
                    Ok(mk.decrypt(&mesg.cipher_text))
                } else {
                    Err(DecryptError::InvalidSignature)
                }
            }
            None => Err(DecryptError::DuplicateMessage)
        }
    }

    fn stage_message_keys<E>(&self, msg: &CipherMessage) -> Result<(ChainKey, MessageKeys, VecDeque<MessageKeys>), DecryptError<E>> {
        let num = (msg.counter.value() - self.chain_key.idx.value()) as usize;

        if num > MAX_COUNTER_GAP {
            return Err(DecryptError::TooDistantFuture)
        }

        let mut buf = VecDeque::with_capacity(num);
        let mut chk = self.chain_key.clone();

        for _ in 0 .. num {
            buf.push_back(chk.message_keys());
            chk = chk.next()
        }

        let mk = chk.message_keys();
        Ok((chk, mk, buf))
    }

    fn commit_message_keys(&mut self, mks: VecDeque<MessageKeys>) {
        assert!(mks.len() <= MAX_COUNTER_GAP);

        let excess = self.message_keys.len() as isize
                   + mks.len() as isize
                   - MAX_COUNTER_GAP as isize;

        for _ in 0 .. excess {
            self.message_keys.pop_front();
        }

        for m in mks.into_iter() {
            self.message_keys.push_back(m)
        }

        assert!(self.message_keys.len() <= MAX_COUNTER_GAP);
    }

    fn encode<W: Write>(&self, e: &mut Encoder<W>) -> EncodeResult<()> {
        try!(e.object(3));
        try!(e.u8(0)); try!(self.chain_key.encode(e));
        try!(e.u8(1)); try!(self.ratchet_key.encode(e));
        try!(e.u8(2));
        {
            try!(e.array(self.message_keys.len()));
            for m in &self.message_keys {
                try!(m.encode(e))
            }
        }
        Ok(())
    }

    fn decode<R: Read + Skip>(d: &mut Decoder<R>) -> DecodeResult<RecvChain> {
        let n = try!(d.object());
        let mut chain_key    = None;
        let mut ratchet_key  = None;
        let mut message_keys = None;
        for _ in 0 .. n {
            match try!(d.u8()) {
                0 => chain_key   = Some(try!(ChainKey::decode(d))),
                1 => ratchet_key = Some(try!(PublicKey::decode(d))),
                2 => {
                    let lv = try!(d.array());
                    let mut vm = VecDeque::with_capacity(lv);
                    for _ in 0 .. lv {
                        vm.push_back(try!(MessageKeys::decode(d)))
                    }
                    message_keys = Some(vm)
                }
                _ => try!(d.skip())
            }
        }
        Ok(RecvChain {
            chain_key:    to_field!(chain_key, "RecvChain::chain_key"),
            ratchet_key:  to_field!(ratchet_key, "RecvChain::ratchet_key"),
            message_keys: message_keys.unwrap_or_else(|| VecDeque::new())
        })
    }
}

// Message Keys /////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct MessageKeys {
    cipher_key: CipherKey,
    mac_key:    MacKey,
    counter:    Counter
}

impl MessageKeys {
    fn encrypt(&self, plain_text: &[u8]) -> Vec<u8> {
        self.cipher_key.encrypt(plain_text, &self.counter.as_nonce())
    }

    fn decrypt(&self, cipher_text: &[u8]) -> Vec<u8> {
        self.cipher_key.decrypt(cipher_text, &self.counter.as_nonce())
    }

    fn encode<W: Write>(&self, e: &mut Encoder<W>) -> EncodeResult<()> {
        try!(e.object(3));
        try!(e.u8(0)); try!(self.cipher_key.encode(e));
        try!(e.u8(1)); try!(self.mac_key.encode(e));
        try!(e.u8(2)); self.counter.encode(e)
    }

    fn decode<R: Read + Skip>(d: &mut Decoder<R>) -> DecodeResult<MessageKeys> {
        let n = try!(d.object());
        let mut cipher_key = None;
        let mut mac_key    = None;
        let mut counter    = None;
        for _ in 0 .. n {
            match try!(d.u8()) {
                0 => cipher_key = Some(try!(CipherKey::decode(d))),
                1 => mac_key    = Some(try!(MacKey::decode(d))),
                2 => counter    = Some(try!(Counter::decode(d))),
                _ => try!(d.skip())
            }
        }
        Ok(MessageKeys {
            cipher_key: to_field!(cipher_key, "MessageKeys::cipher_key"),
            mac_key:    to_field!(mac_key, "MessageKeys::mac_key"),
            counter:    to_field!(counter, "MessageKeys::counter")
        })
    }
}

// Store ////////////////////////////////////////////////////////////////////

pub trait PreKeyStore {
    type Error;

    /// Lookup prekey by ID.
    fn prekey(&mut self, id: PreKeyId) -> Result<Option<PreKey>, Self::Error>;

    /// Remove prekey by ID.
    fn remove(&mut self, id: PreKeyId) -> Result<(), Self::Error>;
}

// Session //////////////////////////////////////////////////////////////////

const MAX_RECV_CHAINS:    usize = 5;
const MAX_SESSION_STATES: usize = 100;

pub struct Indexed<A> {
    pub idx: usize,
    pub val: A
}

impl<A> Indexed<A> {
    pub fn new(i: usize, a: A) -> Indexed<A> {
        Indexed { idx: i, val: a }
    }
}

// Note [session_tag]
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// The session tag denotes the session state which is used to encrypt
// messages. Messages contain the session tag which was used for their
// encryption, which allows the receiving end to perform an efficient
// lookup. It is imperative to ensure that the session tag *always*
// denotes a value in the session's map of session states, otherwise
// `Session::encrypt` can not succeed. The only places where we change
// it after initialisation is in `Session::insert_session_state` which
// sets it to the value of the state which is inserted.
pub struct Session<'r> {
    version:         u8,
    session_tag:     SessionTag,
    counter:         usize,
    local_identity:  &'r IdentityKeyPair,
    remote_identity: IdentityKey,
    pending_prekey:  Option<(PreKeyId, PublicKey)>,
    session_states:  BTreeMap<SessionTag, Indexed<SessionState>>
}

struct AliceParams<'r> {
    alice_ident:   &'r IdentityKeyPair,
    alice_base:    &'r KeyPair,
    bob:           &'r PreKeyBundle
}

struct BobParams<'r> {
    bob_ident:     &'r IdentityKeyPair,
    bob_prekey:    KeyPair,
    alice_ident:   &'r IdentityKey,
    alice_base:    &'r PublicKey
}

impl<'r> Session<'r> {
    pub fn init_from_prekey(alice: &'r IdentityKeyPair, pk: PreKeyBundle) -> Session<'r> {
        let alice_base = KeyPair::new();
        let state      = SessionState::init_as_alice(AliceParams {
            alice_ident: alice,
            alice_base:  &alice_base,
            bob:         &pk
        });

        let session_tag = SessionTag::new();
        let mut session = Session {
            version:         1,
            session_tag:     session_tag,
            counter:         0,
            local_identity:  alice,
            remote_identity: pk.identity_key,
            pending_prekey:  Some((pk.prekey_id, alice_base.public_key)),
            session_states:  BTreeMap::new()
        };

        session.insert_session_state(session_tag, state);
        session
    }

    pub fn init_from_message<S: PreKeyStore>(ours: &'r IdentityKeyPair, store: &mut S, env: &Envelope) -> Result<(Session<'r>, Vec<u8>), DecryptError<S::Error>> {
        let pkmsg = match *env.message() {
            Message::Plain(_)     => return Err(DecryptError::InvalidMessage),
            Message::Keyed(ref m) => m
        };

        let mut session = Session {
            version:         1,
            session_tag:     pkmsg.message.session_tag,
            counter:         0,
            local_identity:  ours,
            remote_identity: (*pkmsg.identity_key).clone(),
            pending_prekey:  None,
            session_states:  BTreeMap::new()
        };

        match try!(session.new_state(store, pkmsg)) {
            Some(mut s) => {
                let plain = try!(s.decrypt(env, &pkmsg.message));
                session.insert_session_state(pkmsg.message.session_tag, s);
                if pkmsg.prekey_id != keys::MAX_PREKEY_ID {
                    try!(store.remove(pkmsg.prekey_id))
                }
                Ok((session, plain))
            }
            None => Err(DecryptError::PreKeyNotFound(pkmsg.prekey_id))
        }
    }

    pub fn encrypt(&mut self, plain: &[u8]) -> EncodeResult<Envelope> {
        let state = try!(self.session_states
                             .get_mut(&self.session_tag)
                             .ok_or(InternalError::NoSessionForTag)); // See note [session_tag]
        state.val.encrypt(&self.local_identity.public_key,
                          &self.pending_prekey,
                          self.session_tag,
                          plain)
    }

    pub fn decrypt<S: PreKeyStore>(&mut self, store: &mut S, env: &Envelope) -> Result<Vec<u8>, DecryptError<S::Error>> {
        match *env.message() {
            Message::Plain(ref m) => self.decrypt_cipher_message(env, m),
            Message::Keyed(ref m) => {
                if *m.identity_key != self.remote_identity {
                    return Err(DecryptError::RemoteIdentityChanged)
                }
                match self.decrypt_cipher_message(env, &m.message) {
                    e @ Err(DecryptError::InvalidSignature) | e @ Err(DecryptError::InvalidMessage) =>
                        match try!(self.new_state(store, m)) {
                            Some(mut s) => {
                                let plain = try!(s.decrypt(env, &m.message));
                                if m.prekey_id != keys::MAX_PREKEY_ID {
                                    try!(store.remove(m.prekey_id))
                                }
                                self.insert_session_state(m.message.session_tag, s);
                                self.pending_prekey = None;
                                Ok(plain)
                            }
                            None => e
                        },
                    x => x
                }
            }
        }
    }

    fn decrypt_cipher_message<E>(&mut self, env: &Envelope, m: &CipherMessage) -> Result<Vec<u8>, DecryptError<E>> {
        let mut s = match self.session_states.get_mut(&m.session_tag) {
            Some(s) => s.val.clone(),
            None    => return Err(DecryptError::InvalidMessage)
        };
        let plain = try!(s.decrypt(env, &m));
        self.pending_prekey = None;
        self.insert_session_state(m.session_tag, s);
        Ok(plain)
    }

    // Attempt to create a new session state based on the prekey that we
    // attempt to lookup in our prekey store. If successful we return the
    // newly created state. It is the caller's responsibility to remove the
    // prekey from the store.
    // See note [no_new_state] for those cases where no prekey has been found.
    fn new_state<S: PreKeyStore>(&self, store: &mut S, m: &PreKeyMessage) -> Result<Option<SessionState>, DecryptError<S::Error>> {
        let s = try!(store.prekey(m.prekey_id)).map(|prekey| {
            SessionState::init_as_bob(BobParams {
                bob_ident:   self.local_identity,
                bob_prekey:  prekey.key_pair,
                alice_ident: &m.identity_key,
                alice_base:  &m.base_key
            })
        });
        Ok(s)
    }

    // Here we either replace a session state we already have with a clone
    // that has ratcheted forward, or we add a new session state. In any
    // case we ensure, that the session's `session_tag` value is equal to
    // the given one.
    //
    // Note [counter_overflow]
    // ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
    // Theoretically the session counter--which is used to give newer session
    // states a higher number than older ones--can overflow. While unlikely,
    // we better handle this gracefully (if somewhat brutal) by clearing all
    // states and resetting the counter to 0. This means that the only session
    // state left is the one to be inserted, but if Alice and Bob do not
    // manage to agree on a session state within `usize::MAX` it is probably
    // of least concern.
    fn insert_session_state(&mut self, t: SessionTag, s: SessionState) {
        if self.session_states.contains_key(&t) {
            self.session_states.get_mut(&t).map(|x| x.val = s);
        } else {
            if self.counter == usize::MAX { // See note [counter_overflow]
                self.session_states.clear();
                self.counter = 0;
            }
            self.session_states.insert(t, Indexed::new(self.counter, s));
            self.counter = self.counter + 1;
        }

        // See note [session_tag]
        if self.session_tag != t {
            self.session_tag = t;
        }

        // Too many states => remove the one with lowest counter value (= oldest)
        if self.session_states.len() >= MAX_SESSION_STATES {
            self.session_states.iter()
                .filter(|s| s.0 != &self.session_tag)
                .min_by_key(|s| s.1.idx)
                .map(|s| s.0.clone())
                .map(|k| self.session_states.remove(&k));
        }
    }

    pub fn local_identity(&self) -> &IdentityKey {
        &self.local_identity.public_key
    }

    pub fn remote_identity(&self) -> &IdentityKey {
        &self.remote_identity
    }

    pub fn serialise(&self) -> EncodeResult<Vec<u8>> {
        let mut e = Encoder::new(Cursor::new(Vec::new()));
        try!(self.encode(&mut e));
        Ok(e.into_writer().into_inner())
    }

    pub fn deserialise(ident: &'r IdentityKeyPair, b: &[u8]) -> DecodeResult<Session<'r>> {
        Session::decode(ident, &mut Decoder::new(Config::default(), Cursor::new(b)))
    }

    pub fn encode<W: Write>(&self, e: &mut Encoder<W>) -> EncodeResult<()> {
        try!(e.object(6));
        try!(e.u8(0)); try!(e.u8(self.version));
        try!(e.u8(1)); try!(self.session_tag.encode(e));
        try!(e.u8(2)); try!(self.local_identity.public_key.encode(e));
        try!(e.u8(3)); try!(self.remote_identity.encode(e));
        try!(e.u8(4));
        {
            match self.pending_prekey {
                None               => try!(e.null()),
                Some((id, ref pk)) => {
                    try!(e.object(2));
                    try!(e.u8(0)); try!(id.encode(e));
                    try!(e.u8(1)); try!(pk.encode(e))
                }
            }
        }
        try!(e.u8(5));
        {
            try!(e.object(self.session_states.len()));
            for (t, s) in &self.session_states {
                try!(t.encode(e));
                try!(s.val.encode(e))
            }
        }
        Ok(())
    }

    pub fn decode<'s, R: Read + Skip>(ident: &'s IdentityKeyPair, d: &mut Decoder<R>) -> DecodeResult<Session<'s>> {
        let n = try!(d.object());
        let mut version         = None;
        let mut session_tag     = None;
        let mut counter         = 0;
        let mut remote_identity = None;
        let mut pending_prekey  = None;
        let mut session_states  = None;
        for _ in 0 .. n {
            match try!(d.u8()) {
                0 => version     = Some(try!(d.u8())),
                1 => session_tag = Some(try!(SessionTag::decode(d))),
                2 => {
                    let li = try!(IdentityKey::decode(d));
                    if ident.public_key != li {
                        return Err(DecodeError::LocalIdentityChanged(li))
                    }
                }
                3 => remote_identity = Some(try!(IdentityKey::decode(d))),
                4 => if let Some(n) = try!(cbor::opt(d.object())) {
                        let mut id = None;
                        let mut pk = None;
                        for _ in 0 .. n {
                            match try!(d.u8()) {
                                0 => id = Some(try!(PreKeyId::decode(d))),
                                1 => pk = Some(try!(PublicKey::decode(d))),
                                _ => try!(d.skip())
                            }
                        }
                        pending_prekey = Some((
                            to_field!(id, "Session::pending_prekey_id"),
                            to_field!(pk, "Session::pending_prekey")
                        ))
                },
                5 => {
                    let ls = try!(d.object());
                    let mut rb = BTreeMap::new();
                    for _ in 0 .. ls {
                        let t = try!(SessionTag::decode(d));
                        let s = try!(SessionState::decode(d));
                        rb.insert(t, Indexed::new(counter, s));
                        counter = counter + 1
                    }
                    session_states = Some(rb)
                }
                _ => try!(d.skip())
            }
        }
        Ok(Session {
            version:         to_field!(version, "Session::version"),
            session_tag:     to_field!(session_tag, "Session::session_tag"),
            counter:         counter,
            local_identity:  ident,
            remote_identity: to_field!(remote_identity, "Session::remote_identity"),
            pending_prekey:  pending_prekey,
            session_states:  to_field!(session_states, "Session::session_states")
        })
    }
}

// Session State ////////////////////////////////////////////////////////////

#[derive(Clone)]
pub struct SessionState {
    recv_chains:  VecDeque<RecvChain>,
    send_chain:   SendChain,
    root_key:     RootKey,
    prev_counter: Counter
}

impl SessionState {
    fn init_as_alice(p: AliceParams) -> SessionState {
        let master_key = {
            let mut buf = Vec::new();
            buf.extend(&p.alice_ident.secret_key.shared_secret(&p.bob.public_key));
            buf.extend(&p.alice_base.secret_key.shared_secret(&p.bob.identity_key.public_key));
            buf.extend(&p.alice_base.secret_key.shared_secret(&p.bob.public_key));
            buf
        };

        let dsecs = DerivedSecrets::kdf_without_salt(Input(&master_key), Info(b"handshake"));

        // receiving chain
        let rootkey  = RootKey::from_cipher_key(dsecs.cipher_key);
        let chainkey = ChainKey::from_mac_key(dsecs.mac_key, Counter::zero());

        let mut recv_chains = VecDeque::with_capacity(MAX_RECV_CHAINS + 1);
        recv_chains.push_front(RecvChain::new(chainkey, p.bob.public_key.clone()));

        // sending chain
        let send_ratchet = KeyPair::new();
        let (rok, chk)   = rootkey.dh_ratchet(&send_ratchet, &p.bob.public_key);
        let send_chain   = SendChain::new(chk, send_ratchet);

        SessionState {
            recv_chains:     recv_chains,
            send_chain:      send_chain,
            root_key:        rok,
            prev_counter:    Counter::zero()
        }
    }

    fn init_as_bob(p: BobParams) -> SessionState {
        let master_key = {
            let mut buf = Vec::new();
            buf.extend(&p.bob_prekey.secret_key.shared_secret(&p.alice_ident.public_key));
            buf.extend(&p.bob_ident.secret_key.shared_secret(p.alice_base));
            buf.extend(&p.bob_prekey.secret_key.shared_secret(p.alice_base));
            buf
        };

        let dsecs = DerivedSecrets::kdf_without_salt(Input(&master_key), Info(b"handshake"));

        // sending chain
        let rootkey    = RootKey::from_cipher_key(dsecs.cipher_key);
        let chainkey   = ChainKey::from_mac_key(dsecs.mac_key, Counter::zero());
        let send_chain = SendChain::new(chainkey, p.bob_prekey);

        SessionState {
            recv_chains:     VecDeque::with_capacity(MAX_RECV_CHAINS + 1),
            send_chain:      send_chain,
            root_key:        rootkey,
            prev_counter:    Counter::zero()
        }
    }

    fn ratchet(&mut self, ratchet_key: PublicKey) {
        let new_ratchet = KeyPair::new();

        let (recv_root_key, recv_chain_key) =
            self.root_key.dh_ratchet(&self.send_chain.ratchet_key, &ratchet_key);

        let (send_root_key, send_chain_key) =
            recv_root_key.dh_ratchet(&new_ratchet, &ratchet_key);

        let recv_chain    = RecvChain::new(recv_chain_key, ratchet_key);
        let send_chain    = SendChain::new(send_chain_key, new_ratchet);
        self.root_key     = send_root_key;
        self.prev_counter = self.send_chain.chain_key.idx;
        self.send_chain   = send_chain;

        self.recv_chains.push_front(recv_chain);

        if self.recv_chains.len() > MAX_RECV_CHAINS {
            self.recv_chains.pop_back();
        }
    }

    fn encrypt<'r>(self:    &'r mut SessionState,
                   ident:   &'r IdentityKey,
                   pending: &'r Option<(PreKeyId, PublicKey)>,
                   tag:     SessionTag,
                   plain:   &[u8]) -> EncodeResult<Envelope>
    {
        let msgkeys = self.send_chain.chain_key.message_keys();

        let cmessage = CipherMessage {
            session_tag:  tag,
            ratchet_key:  Cow::Borrowed(&self.send_chain.ratchet_key.public_key),
            counter:      self.send_chain.chain_key.idx,
            prev_counter: self.prev_counter,
            cipher_text:  msgkeys.encrypt(plain)
        };

        let message = match *pending {
            None         => Message::Plain(cmessage),
            Some(ref pp) => Message::Keyed(PreKeyMessage {
                prekey_id:    pp.0,
                base_key:     Cow::Borrowed(&pp.1),
                identity_key: Cow::Borrowed(&ident),
                message:      cmessage
            })
        };

        let env = Envelope::new(&msgkeys.mac_key, message);
        self.send_chain.chain_key = self.send_chain.chain_key.next();
        env
    }

    fn decrypt<E>(&mut self, env: &Envelope, m: &CipherMessage) -> Result<Vec<u8>, DecryptError<E>> {
        let mut rchain =
            match self.recv_chains.iter().position(|c| c.ratchet_key == *m.ratchet_key) {
                Some(i) => &mut self.recv_chains[i],
                None    => {
                    self.ratchet((*m.ratchet_key).clone());
                    &mut self.recv_chains[0]
                }
            };

        match m.counter.cmp(&rchain.chain_key.idx) {
            Ordering::Less    => rchain.try_message_keys(env, m),
            Ordering::Greater => {
                let (chk, mk, mks) = try!(rchain.stage_message_keys(m));
                if !env.verify(&mk.mac_key) {
                    return Err(DecryptError::InvalidSignature)
                }
                let plain = mk.decrypt(&m.cipher_text);
                rchain.chain_key = chk.next();
                rchain.commit_message_keys(mks);
                Ok(plain)
            }
            Ordering::Equal => {
                let mks = rchain.chain_key.message_keys();
                if !env.verify(&mks.mac_key) {
                    return Err(DecryptError::InvalidSignature)
                }
                let plain = mks.decrypt(&m.cipher_text);
                rchain.chain_key = rchain.chain_key.next();
                Ok(plain)
            }
        }
    }

    fn encode<W: Write>(&self, e: &mut Encoder<W>) -> EncodeResult<()> {
        try!(e.object(4));
        try!(e.u8(0));
        {
            try!(e.array(self.recv_chains.len()));
            for r in &self.recv_chains {
                try!(r.encode(e))
            }
        }
        try!(e.u8(1)); try!(self.send_chain.encode(e));
        try!(e.u8(2)); try!(self.root_key.encode(e));
        try!(e.u8(3)); try!(self.prev_counter.encode(e));
        // Note that key '4' was used for skipped message keys.
        Ok(())
    }

    fn decode<R: Read + Skip>(d: &mut Decoder<R>) -> DecodeResult<SessionState> {
        let n = try!(d.object());
        let mut recv_chains     = None;
        let mut send_chain      = None;
        let mut root_key        = None;
        let mut prev_counter    = None;
        for _ in 0 .. n {
            match try!(d.u8()) {
                0 => {
                    let lr = try!(d.array());
                    let mut rr = VecDeque::with_capacity(lr);
                    for _ in 0 .. lr {
                        rr.push_back(try!(RecvChain::decode(d)))
                    }
                    recv_chains = Some(rr)
                }
                1 => send_chain   = Some(try!(SendChain::decode(d))),
                2 => root_key     = Some(try!(RootKey::decode(d))),
                3 => prev_counter = Some(try!(Counter::decode(d))),
                _ => try!(d.skip())
            }
        }
        Ok(SessionState {
            recv_chains:     to_field!(recv_chains, "SessionState::recv_chains"),
            send_chain:      to_field!(send_chain, "SessionState::send_chain"),
            root_key:        to_field!(root_key, "SessionState::root_key"),
            prev_counter:    to_field!(prev_counter, "SessionState::prev_counter")
        })
    }
}

// Decrypt Error ////////////////////////////////////////////////////////////

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum DecryptError<E> {
    RemoteIdentityChanged,
    InvalidSignature,
    InvalidMessage,
    DuplicateMessage,
    TooDistantFuture,
    OutdatedMessage,
    PreKeyNotFound(PreKeyId),
    PreKeyStoreError(E)
}

impl<E> DecryptError<E> {
    fn as_str(&self) -> &str {
        match *self {
            DecryptError::RemoteIdentityChanged => "RemoteIdentityChanged",
            DecryptError::InvalidSignature      => "InvalidSignature",
            DecryptError::InvalidMessage        => "InvalidMessage",
            DecryptError::DuplicateMessage      => "DuplicateMessage",
            DecryptError::TooDistantFuture      => "TooDistantFuture",
            DecryptError::OutdatedMessage       => "OutdatedMessage",
            DecryptError::PreKeyNotFound(_)     => "PreKeyNotFound",
            DecryptError::PreKeyStoreError(_)   => "PreKeyStoreError"
        }
    }
}

impl<E: fmt::Debug> fmt::Debug for DecryptError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            DecryptError::PreKeyStoreError(ref e) => write!(f, "PreKeyStoreError: {:?}", e),
            DecryptError::PreKeyNotFound(i)       => write!(f, "PreKeyNotFound: {:?}", i),
            _                                     => f.write_str(self.as_str())
        }
    }
}

impl<E: fmt::Display> fmt::Display for DecryptError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            DecryptError::PreKeyStoreError(ref e) => write!(f, "PreKeyStoreError: {}", e),
            DecryptError::PreKeyNotFound(i)       => write!(f, "PreKeyNotFound: {}", i),
            _                                     => f.write_str(self.as_str())
        }
    }
}

impl<E: Error> Error for DecryptError<E> {
    fn description(&self) -> &str {
        self.as_str()
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            DecryptError::PreKeyStoreError(ref e) => Some(e),
            _                                     => None
        }
    }
}

impl<E> From<E> for DecryptError<E> {
    fn from(e: E) -> DecryptError<E> {
        DecryptError::PreKeyStoreError(e)
    }
}

// Tests ////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use internal::keys::{IdentityKeyPair, PreKey, PreKeyId, PreKeyBundle, PreKeyAuth};
    use internal::keys::gen_prekeys;
    use internal::message::{Counter, Envelope, Message, SessionTag};
    use std::collections::BTreeMap;
    use std::fmt;
    use std::usize;
    use std::vec::Vec;
    use super::*;

    struct TestStore {
        prekeys: Vec<PreKey>
    }

    impl TestStore {
        pub fn prekey_slice(&self) -> &[PreKey] {
            &self.prekeys
        }
    }

    impl PreKeyStore for TestStore {
        type Error = ();

        fn prekey(&mut self, id: PreKeyId) -> Result<Option<PreKey>, ()> {
            Ok(self.prekeys.iter().find(|k| k.key_id == id).map(|k| k.clone()))
        }

        fn remove(&mut self, id: PreKeyId) -> Result<(), ()> {
            self.prekeys.iter()
                .position(|k| k.key_id == id)
                .map(|ix| self.prekeys.swap_remove(ix));
            Ok(())
        }
    }

    #[derive(Debug, Copy, Clone, PartialEq)]
    enum MsgType { Plain, Keyed }

    #[test]
    fn pathological_case() {
        let total_size = 32;

        let alice_ident   = IdentityKeyPair::new();
        let bob_ident     = IdentityKeyPair::new();
        let mut bob_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), total_size as u16) };

        let mut alices = Vec::new();
        for pk in bob_store.prekey_slice() {
            let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), pk);
            alices.push(Session::init_from_prekey(&alice_ident, bob_bundle));
        }

        assert_eq!(total_size, alices.len());

        let mut bob = Session::init_from_message(&bob_ident, &mut bob_store, &alices[0].encrypt(b"hello").unwrap().into_owned()).unwrap().0;

        for a in &mut alices {
            for _ in 0 .. 900 { // Inflate `MessageKeys` vector
                let _ = a.encrypt(b"hello").unwrap();
            }
            let hello_bob = a.encrypt(b"Hello Bob!").unwrap();
            assert!(bob.decrypt(&mut bob_store, &hello_bob).is_ok())
        }

        assert_eq!(total_size, bob.session_states.len());

        for a in &mut alices {
            assert!(bob.decrypt(&mut bob_store, &a.encrypt(b"Hello Bob!").unwrap()).is_ok());
        }
    }

    #[test]
    fn encrypt_decrypt() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        alice = Session::deserialise(&alice_ident, &alice.serialise().unwrap())
                        .unwrap_or_else(|e| panic!("Failed to decode session: {}", e));
        assert_eq!(1, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains.len());

        let hello_bob = alice.encrypt(b"Hello Bob!").unwrap().into_owned();
        let hello_bob_delayed = alice.encrypt(b"Hello delay!").unwrap().into_owned();
        assert_eq!(1, alice.session_states.len());
        assert_eq!(1, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains.len());

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");
        bob = Session::deserialise(&bob_ident, &bob.serialise().unwrap())
                      .unwrap_or_else(|e| panic!("Failed to decode session: {}", e));
        assert_eq!(1, bob.session_states.len());
        assert_eq!(1, bob.session_states.get(&bob.session_tag).unwrap().val.recv_chains.len());
        assert_eq!(bob.remote_identity.fingerprint(), alice.local_identity.public_key.fingerprint());

        let hello_alice = bob.encrypt(b"Hello Alice!").unwrap().into_owned();

        // Alice
        assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &hello_alice));
        assert!(alice.pending_prekey.is_none());
        assert_eq!(2, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains.len());
        assert_eq!(alice.remote_identity.fingerprint(), bob.local_identity.public_key.fingerprint());
        let ping_bob_1 = alice.encrypt(b"Ping1!").unwrap().into_owned();
        assert_is_msg(&ping_bob_1, MsgType::Plain);
        let ping_bob_2 = alice.encrypt(b"Ping2!").unwrap().into_owned();
        assert_is_msg(&ping_bob_2, MsgType::Plain);
        assert_prev_count(&alice, 2);

        // Bob
        assert_decrypt(b"Ping1!", bob.decrypt(&mut bob_store, &ping_bob_1));
        assert_eq!(2, bob.session_states.get(&bob.session_tag).unwrap().val.recv_chains.len());
        assert_decrypt(b"Ping2!", bob.decrypt(&mut bob_store, &ping_bob_2));
        assert_eq!(2, bob.session_states.get(&bob.session_tag).unwrap().val.recv_chains.len());
        let pong_alice = bob.encrypt(b"Pong!").unwrap().into_owned();
        assert_prev_count(&bob, 1);

        // Alice
        assert_decrypt(b"Pong!", alice.decrypt(&mut alice_store, &pong_alice));
        assert_eq!(3, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains.len());
        assert_prev_count(&alice, 2);

        // Bob (Delayed (prekey) message, decrypted with the "old" receive chain)
        assert_decrypt(b"Hello delay!", bob.decrypt(&mut bob_store, &hello_bob_delayed));
        assert_eq!(2, bob.session_states.get(&bob.session_tag).unwrap().val.recv_chains.len());
        assert_prev_count(&bob, 1);
    }

    #[test]
    fn counter_mismatch() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!").unwrap().into_owned();

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");

        let hello1 = bob.encrypt(b"Hello1").unwrap().into_owned();
        let hello2 = bob.encrypt(b"Hello2").unwrap().into_owned();
        let hello3 = bob.encrypt(b"Hello3").unwrap().into_owned();
        let hello4 = bob.encrypt(b"Hello4").unwrap().into_owned();
        let hello5 = bob.encrypt(b"Hello5").unwrap().into_owned();

        assert_decrypt(b"Hello2", alice.decrypt(&mut alice_store, &hello2));
        assert_eq!(1, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains[0].message_keys.len());

        assert_decrypt(b"Hello1", alice.decrypt(&mut alice_store, &hello1));
        assert_eq!(0, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains[0].message_keys.len());

        assert_decrypt(b"Hello3", alice.decrypt(&mut alice_store, &hello3));
        assert_eq!(0, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains[0].message_keys.len());

        assert_decrypt(b"Hello5", alice.decrypt(&mut alice_store, &hello5));
        assert_eq!(1, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains[0].message_keys.len());

        assert_decrypt(b"Hello4", alice.decrypt(&mut alice_store, &hello4));
        assert_eq!(0, alice.session_states.get(&alice.session_tag).unwrap().val.recv_chains[0].message_keys.len());

        for m in &vec![hello1, hello2, hello3, hello4, hello5] {
            assert_eq!(Some(DecryptError::DuplicateMessage), alice.decrypt(&mut alice_store, m).err());
        }
    }

    #[test]
    fn multiple_prekey_msgs() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut bob_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice  = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob1 = alice.encrypt(b"Hello Bob1!").unwrap().into_owned();
        let hello_bob2 = alice.encrypt(b"Hello Bob2!").unwrap().into_owned();
        let hello_bob3 = alice.encrypt(b"Hello Bob3!").unwrap().into_owned();

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob1, b"Hello Bob1!");
        assert_eq!(1, bob.session_states.len());
        assert_decrypt(b"Hello Bob2!", bob.decrypt(&mut bob_store, &hello_bob2));
        assert_eq!(1, bob.session_states.len());
        assert_decrypt(b"Hello Bob3!", bob.decrypt(&mut bob_store, &hello_bob3));
        assert_eq!(1, bob.session_states.len());
    }

    #[test]
    fn simultaneous_prekey_msgs() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let alice_prekey = alice_store.prekey_slice().first().unwrap().clone();
        let alice_bundle = PreKeyBundle::new(alice_ident.public_key.clone(), &alice_prekey);

        // Initial simultaneous prekey message
        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!").unwrap().into_owned();
        assert_is_msg(&hello_bob, MsgType::Keyed);

        let mut bob     = Session::init_from_prekey(&bob_ident, alice_bundle);
        let hello_alice = bob.encrypt(b"Hello Alice!").unwrap().into_owned();
        assert_is_msg(&hello_alice, MsgType::Keyed);

        assert_decrypt(b"Hello Bob!", bob.decrypt(&mut bob_store, &hello_bob));
        assert_eq!(2, bob.session_states.len());

        assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &hello_alice));
        assert_eq!(2, alice.session_states.len());

        // Non-simultaneous answer, which results in agreement of a session.
        let greet_bob = alice.encrypt(b"That was fast!").unwrap().into_owned();
        assert_is_msg(&greet_bob, MsgType::Plain);
        assert_decrypt(b"That was fast!", bob.decrypt(&mut bob_store, &greet_bob));

        let answer_alice = bob.encrypt(b":-)").unwrap().into_owned();
        assert_is_msg(&answer_alice, MsgType::Plain);
        assert_decrypt(b":-)", alice.decrypt(&mut alice_store, &answer_alice));
    }

    #[test]
    fn simultaneous_msgs_repeated() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let alice_prekey = alice_store.prekey_slice().first().unwrap().clone();
        let alice_bundle = PreKeyBundle::new(alice_ident.public_key.clone(), &alice_prekey);

        // Initial simultaneous prekey message
        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!").unwrap().into_owned();
        assert_is_msg(&hello_bob, MsgType::Keyed);

        let mut bob     = Session::init_from_prekey(&bob_ident, alice_bundle);
        let hello_alice = bob.encrypt(b"Hello Alice!").unwrap().into_owned();
        assert_is_msg(&hello_alice, MsgType::Keyed);

        assert_decrypt(b"Hello Bob!", bob.decrypt(&mut bob_store, &hello_bob));
        assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &hello_alice));

        // Second simultaneous message
        let echo_bob1 = alice.encrypt(b"Echo Bob1!").unwrap().into_owned();
        assert_is_msg(&echo_bob1, MsgType::Plain);

        let echo_alice1 = bob.encrypt(b"Echo Alice1!").unwrap().into_owned();
        assert_is_msg(&echo_alice1, MsgType::Plain);

        assert_decrypt(b"Echo Bob1!", bob.decrypt(&mut bob_store, &echo_bob1));
        assert_eq!(2, bob.session_states.len());

        assert_decrypt(b"Echo Alice1!", alice.decrypt(&mut alice_store, &echo_alice1));
        assert_eq!(2, alice.session_states.len());

        // Third simultaneous message
        let echo_bob2 = alice.encrypt(b"Echo Bob2!").unwrap().into_owned();
        assert_is_msg(&echo_bob2, MsgType::Plain);

        let echo_alice2 = bob.encrypt(b"Echo Alice2!").unwrap().into_owned();
        assert_is_msg(&echo_alice2, MsgType::Plain);

        assert_decrypt(b"Echo Bob2!", bob.decrypt(&mut bob_store, &echo_bob2));
        assert_eq!(2, bob.session_states.len());

        assert_decrypt(b"Echo Alice2!", alice.decrypt(&mut alice_store, &echo_alice2));
        assert_eq!(2, alice.session_states.len());

        // Non-simultaneous answer, which results in agreement of a session.
        let stop_bob = alice.encrypt(b"Stop it!").unwrap().into_owned();
        assert_decrypt(b"Stop it!", bob.decrypt(&mut bob_store, &stop_bob));

        let answer_alice = bob.encrypt(b"OK").unwrap().into_owned();
        assert_decrypt(b"OK", alice.decrypt(&mut alice_store, &answer_alice));
    }

    #[test]
    fn enc_dec_session() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let bob_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let bytes = alice.serialise().unwrap();

        match Session::deserialise(&alice_ident, &bytes) {
            Err(ref e)        => panic!("Failed to decode session: {}", e),
            Ok(s@Session{..}) => assert_eq!(bytes, s.serialise().unwrap())
        };
    }

    #[test]
    fn mass_communication() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!").unwrap().into_owned();

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");

        let mut buffer = Vec::with_capacity(1000);
        for _ in 0 .. 1000 {
            buffer.push(bob.encrypt(b"Hello Alice!").unwrap().serialise().unwrap())
        }

        for msg in &buffer {
            assert_decrypt(b"Hello Alice!", alice.decrypt(&mut alice_store, &Envelope::deserialise(msg).unwrap()));
        }
    }

    #[test]
    fn retry_init_from_message() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut bob_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!").unwrap();

        assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");
        // The behavior on retry depends on the PreKeyStore implementation.
        // With a PreKeyStore that eagerly deletes prekeys, like the TestStore,
        // the prekey will be gone and a retry cause an error (and thus a lost message).
        match Session::init_from_message(&bob_ident, &mut bob_store, &hello_bob) {
            Err(DecryptError::PreKeyNotFound(_)) => {} // expected
            Err(e) => { panic!(format!("{:?}", e)) }
            Ok(_)  => { panic!("Unexpected success on retrying init_from_message") }
        }
    }

    #[test]
    fn skipped_message_keys() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut alice_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let mut bob_store   = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob = alice.encrypt(b"Hello Bob!").unwrap().into_owned();

        {
            let ref s = alice.session_states.get(&alice.session_tag).unwrap().val;
            assert_eq!(1, s.recv_chains.len());
            assert_eq!(Counter::zero(), s.recv_chains[0].chain_key.idx);
            assert_eq!(Counter::zero().next(), s.send_chain.chain_key.idx);
            assert_eq!(0, s.recv_chains[0].message_keys.len())
        }

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob, b"Hello Bob!");

        {
            // Normal exchange. Bob has created a new receive chain without skipped message keys.
            let ref s = bob.session_states.get(&bob.session_tag).unwrap().val;
            assert_eq!(1, s.recv_chains.len());
            assert_eq!(Counter::zero().next(), s.recv_chains[0].chain_key.idx);
            assert_eq!(Counter::zero(), s.send_chain.chain_key.idx);
            assert_eq!(0, s.recv_chains[0].message_keys.len())
        }

        let hello_alice0 = bob.encrypt(b"Hello0").unwrap().into_owned();
        let _            = bob.encrypt(b"Hello1").unwrap().into_owned();
        let hello_alice2 = bob.encrypt(b"Hello2").unwrap().into_owned();
        assert_decrypt(b"Hello2", alice.decrypt(&mut alice_store, &hello_alice2));

        {
            // Alice has two skipped message keys in her new receive chain.
            let ref s = alice.session_states.get(&alice.session_tag).unwrap().val;
            assert_eq!(2, s.recv_chains.len());
            assert_eq!(Counter::zero().next().next().next(), s.recv_chains[0].chain_key.idx);
            assert_eq!(Counter::zero(), s.send_chain.chain_key.idx);
            assert_eq!(2, s.recv_chains[0].message_keys.len());
            assert_eq!(0, s.recv_chains[0].message_keys[0].counter.value());
            assert_eq!(1, s.recv_chains[0].message_keys[1].counter.value())
        }

        let hello_bob0 = alice.encrypt(b"Hello0").unwrap().into_owned();
        assert_decrypt(b"Hello0", bob.decrypt(&mut bob_store, &hello_bob0));

        {
            // For Bob everything is normal still. A new message from Alice means a
            // new receive chain has been created and again no skipped message keys.
            let ref s = bob.session_states.get(&bob.session_tag).unwrap().val;
            assert_eq!(2, s.recv_chains.len());
            assert_eq!(Counter::zero().next(), s.recv_chains[0].chain_key.idx);
            assert_eq!(Counter::zero(), s.send_chain.chain_key.idx);
            assert_eq!(0, s.recv_chains[0].message_keys.len())
        }

        assert_decrypt(b"Hello0", alice.decrypt(&mut alice_store, &hello_alice0));

        {
            // Alice received the first of the two missing messages. Therefore
            // only one message key is still skipped (counter value = 1).
            let ref s = alice.session_states.get(&alice.session_tag).unwrap().val;
            assert_eq!(2, s.recv_chains.len());
            assert_eq!(1, s.recv_chains[0].message_keys.len());
            assert_eq!(1, s.recv_chains[0].message_keys[0].counter.value())
        }


        let hello_again0 = bob.encrypt(b"Again0").unwrap().into_owned();
        let hello_again1 = bob.encrypt(b"Again1").unwrap().into_owned();

        assert_decrypt(b"Again1", alice.decrypt(&mut alice_store, &hello_again1));

        {
            // Bob has sent two new messages which Alice receives out of order.
            // The first one received causes a new ratchet and hence a new receive chain.
            // The second one will cause Alice to look into her skipped message keys since
            // the message index is lower than the receive chain index. This test therefore
            // ensures that skipped message keys are local to receive chains since the previous
            // receive chain still has a skipped message key with an index > 0 which would
            // cause an `OutdatedMessage` error if the vector was shared across receive chains.
            let ref s = alice.session_states.get(&alice.session_tag).unwrap().val;
            assert_eq!(3, s.recv_chains.len());
            assert_eq!(1, s.recv_chains[0].message_keys.len());
            assert_eq!(1, s.recv_chains[1].message_keys.len());
            assert_eq!(0, s.recv_chains[0].message_keys[0].counter.value());
            assert_eq!(1, s.recv_chains[1].message_keys[0].counter.value());
        }

        assert_decrypt(b"Again0", alice.decrypt(&mut alice_store, &hello_again0))
    }

    #[test]
    fn signed_prekeys() {
        let bob_ident = IdentityKeyPair::new();
        let eve_ident = IdentityKeyPair::new();

        let eve_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };

        let     eve_prekey        = eve_store.prekey_slice().first().unwrap().clone();
        let mut eve_bundle        = PreKeyBundle::new(eve_ident.public_key.clone(), &eve_prekey);
        let mut eve_bundle_signed = PreKeyBundle::signed(&eve_ident, &eve_prekey);

        // eve uses her own ephemeral keypair but tries to use bob's identity
        // (e.g. to benefit from existing trust relationships)
        eve_bundle_signed.identity_key = bob_ident.public_key.clone();
        eve_bundle.identity_key        = bob_ident.public_key.clone();

        // non-authentic prekeys
        assert_eq!(PreKeyAuth::Unknown, eve_bundle.verify());
        assert_eq!(PreKeyAuth::Invalid, eve_bundle_signed.verify());

        // authentic prekey
        let bob_store  = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 10) };
        let bob_prekey = bob_store.prekey_slice().first().unwrap().clone();
        let bob_bundle_signed = PreKeyBundle::signed(&bob_ident, &bob_prekey);
        assert_eq!(PreKeyAuth::Valid, bob_bundle_signed.verify());
    }

    #[test]
    fn session_states_limit() {
        let alice = IdentityKeyPair::new();
        let bob   = IdentityKeyPair::new();

        let mut bob_store = TestStore { prekeys: gen_prekeys(PreKeyId::new(0), 500) };

        let get_bob = |i, store: &mut TestStore| {
            PreKeyBundle::new(bob.public_key.clone(), &store.prekey(i).unwrap().unwrap())
        };

        let mut alice2bob = Session::init_from_prekey(&alice, get_bob(PreKeyId::new(1), &mut bob_store));
        let mut hello_bob = alice2bob.encrypt(b"Hello Bob!").unwrap().into_owned();
        assert_is_msg(&hello_bob, MsgType::Keyed);

        let mut bob2alice = Session::init_from_message(&bob, &mut bob_store, &hello_bob).unwrap().0;
        assert_eq!(1, bob2alice.session_states.len());

        // find oldest session state
        let oldest = |m: &BTreeMap<SessionTag, Indexed<SessionState>>| {
            let mut x = SessionTag::new();
            let mut n = usize::MAX;
            for (k, v) in m {
                if v.idx < n {
                    n = v.idx;
                    x = k.clone()
                }
            }
            x
        };

        for i in 2 .. 500 {
            alice2bob = Session::init_from_prekey(&alice, get_bob(PreKeyId::new(i), &mut bob_store));
            hello_bob = alice2bob.encrypt(b"Hello Bob!").unwrap().into_owned();
            assert_is_msg(&hello_bob, MsgType::Keyed);

            let to_remove = oldest(&bob2alice.session_states);
            assert_decrypt(b"Hello Bob!", bob2alice.decrypt(&mut bob_store, &hello_bob));
            let n = bob2alice.session_states.len();
            assert!(n < 100);
            if i > 99 {
                assert_eq!(false, bob2alice.session_states.contains_key(&to_remove))
            }
        }
    }

    #[test]
    fn replaced_prekeys() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut bob_store1 = TestStore { prekeys: vec![PreKey::new(PreKeyId::new(1))] };
        let mut bob_store2 = TestStore { prekeys: vec![PreKey::new(PreKeyId::new(1))] };

        let bob_prekey = bob_store1.prekey(PreKeyId::new(1)).unwrap().unwrap();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice  = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob1 = alice.encrypt(b"Hello Bob1!").unwrap().into_owned();

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store1, &hello_bob1, b"Hello Bob1!");
        assert_eq!(1, bob.session_states.len());

        let hello_bob2 = alice.encrypt(b"Hello Bob2!").unwrap().into_owned();
        assert_decrypt(b"Hello Bob2!", bob.decrypt(&mut bob_store1, &hello_bob2));
        assert_eq!(1, bob.session_states.len());

        let hello_bob3 = alice.encrypt(b"Hello Bob3!").unwrap().into_owned();
        assert_decrypt(b"Hello Bob3!", bob.decrypt(&mut bob_store2, &hello_bob3));
        assert_eq!(1, bob.session_states.len());
    }

    #[test]
    fn max_counter_gap() {
        let alice_ident = IdentityKeyPair::new();
        let bob_ident   = IdentityKeyPair::new();

        let mut bob_store = TestStore { prekeys: vec![PreKey::last_resort()] };

        let bob_prekey = bob_store.prekey(PreKeyId::new(0xFFFF)).unwrap().unwrap();
        let bob_bundle = PreKeyBundle::new(bob_ident.public_key.clone(), &bob_prekey);

        let mut alice  = Session::init_from_prekey(&alice_ident, bob_bundle);
        let hello_bob1 = alice.encrypt(b"Hello Bob!").unwrap().into_owned();

        let mut bob = assert_init_from_message(&bob_ident, &mut bob_store, &hello_bob1, b"Hello Bob!");
        assert_eq!(1, bob.session_states.len());

        for _ in 0 .. 1001 {
            let hello_bob2 = alice.encrypt(b"Hello Bob!").unwrap().into_owned();
            assert_decrypt(b"Hello Bob!", bob.decrypt(&mut bob_store, &hello_bob2));
            assert_eq!(1, bob.session_states.len());
        }
    }

    fn assert_decrypt<E>(expected: &[u8], actual: Result<Vec<u8>, DecryptError<E>>)
        where E: fmt::Debug
    {
        match actual {
            Ok(b)  => {
                let r: &[u8] = b.as_ref();
                assert_eq!(expected, r)
            },
            Err(e) => assert!(false, format!("{:?}", e))
        }
    }

    fn assert_init_from_message<'r, S>(i: &'r IdentityKeyPair, s: &mut S, m: &Envelope, t: &[u8]) -> Session<'r>
        where S: PreKeyStore,
              S::Error: fmt::Debug
    {
        match Session::init_from_message(i, s, m) {
            Ok((s, b)) => {
                let r: &[u8] = b.as_ref();
                assert_eq!(t, r);
                s
            },
            Err(e) => {
                assert!(false, format!("{:?}", e));
                unreachable!()
            }
        }
    }

    fn assert_prev_count(s: &Session, expected: u32) {
        assert_eq!(expected, s.session_states.get(&s.session_tag).unwrap().val.prev_counter.value());
    }

    fn assert_is_msg(e: &Envelope, t: MsgType) {
        match *e.message() {
            Message::Plain(_) if t == MsgType::Plain => (),
            Message::Keyed(_) if t == MsgType::Keyed => {},
            _ => panic!("invalid message type")
        }
    }
}
