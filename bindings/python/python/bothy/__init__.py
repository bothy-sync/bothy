"""Bothy: serverless, local-first sync over your own Google Drive.

Every device keeps a local SQLite database and works offline; devices exchange immutable files
through the user's own Google Drive (or a shared folder). Concurrent edits to different fields
merge; the files on Drive can be end-to-end encrypted.

>>> import bothy
>>> db = bothy.Database.open("robot.db", app_id="com.example.robot",
...                          remote=bothy.FolderRemote("/mnt/shared"))  # doctest: +SKIP
>>> db.init_or_join()                                                # doctest: +SKIP
>>> db.collection("missions").put("m1", {"goal": "dock"})            # doctest: +SKIP
>>> db.sync()                                                        # doctest: +SKIP

The API is synchronous and thread-safe; calls release the GIL while the engine works.
"""

import json as _json
from typing import Any, Callable, Dict, Iterator, List, Optional, Tuple, Union

from . import _native, auth
from ._hlc import Hlc
from ._native import (
    AuthRequiredError,
    BothyError,
    CorruptError,
    DatabaseLockedError,
    FormatTooNewError,
    InternalError,
    InvalidInputError,
    KeyRequiredError,
    OfflineError,
    QuotaExceededError,
    RateLimitedError,
    RemoteInconsistentError,
    SchemaTooNewError,
    WrongPasswordError,
)

__all__ = [
    "FORMAT_VERSION",
    "AuthRequiredError",
    "BothyError",
    "Change",
    "Collection",
    "CorruptError",
    "Database",
    "DatabaseLockedError",
    "DriveRemote",
    "FolderRemote",
    "FormatTooNewError",
    "Hlc",
    "InternalError",
    "InvalidInputError",
    "KeyRequiredError",
    "OfflineError",
    "QuotaExceededError",
    "RateLimitedError",
    "RemoteInconsistentError",
    "SchemaTooNewError",
    "WrongPasswordError",
    "__version__",
    "auth",
]

__version__ = _native.__version__

#: Version of the on-remote file format (``bothy_format`` in ``manifest.json``).
FORMAT_VERSION = 1

Document = Dict[str, Any]
#: A token source for Google Drive: ``bothy.auth.TokenProvider`` or ``(force_refresh) -> str``.
Tokens = Union["_native.TokenProvider", Callable[[bool], str]]


def FolderRemote(path: str) -> "_native.Remote":
    """A local or shared directory as the remote."""
    return _native.Remote.folder(str(path))


class DriveRemote:
    """Google Drive remotes. ``tokens`` gives access tokens (see ``bothy.auth``)."""

    @staticmethod
    def appdata(tokens: Tokens) -> "_native.Remote":
        """The app's hidden folder (``drive.appdata`` scope; not available to the device flow)."""
        return _native.Remote.drive_appdata(tokens)

    @staticmethod
    def folder(name: str, tokens: Tokens) -> "_native.Remote":
        """A visible folder ``Bothy/<name>`` (``drive.file`` scope; works with the device flow)."""
        return _native.Remote.drive_folder(name, tokens)


def _dumps(doc: Document) -> str:
    if not isinstance(doc, dict):
        raise TypeError("a document must be a dict")
    return _json.dumps(doc, allow_nan=False)


class Change(Tuple[str, str, bool]):
    """A changed document: ``(collection, id, remote)``."""

    def __repr__(self) -> str:
        return (f"Change(collection={self.collection!r}, id={self.id!r}, "
                f"remote={self.remote!r})")

    @property
    def collection(self) -> str:
        return self[0]

    @property
    def id(self) -> str:
        return self[1]

    @property
    def remote(self) -> bool:
        """True if the change came from another device."""
        return self[2]


class Collection:
    """A set of documents (JSON objects) addressed by id."""

    def __init__(self, db: "Database", name: str) -> None:
        self._db = db
        self.name = name

    def put(self, id: str, doc: Document) -> bool:
        """Replaces the document. Returns whether anything changed."""
        return self._db._native.put(self.name, id, _dumps(doc))

    def patch(self, id: str, fields: Document, unset: Optional[List[str]] = None) -> bool:
        """Sets ``fields`` and removes the fields in ``unset``."""
        return self._db._native.patch(self.name, id, _dumps(fields), list(unset or []))

    def get(self, id: str) -> Optional[Document]:
        """The document, or ``None``."""
        text = self._db._native.get(self.name, id)
        return None if text is None else _json.loads(text)

    def list(self, limit: int = 100, after: Optional[str] = None) -> List[Tuple[str, Document]]:
        """Up to ``limit`` ``(id, document)`` pairs in id order, after the id ``after``."""
        return [(i, _json.loads(t)) for i, t in self._db._native.list(self.name, limit, after)]

    def all(self, page: int = 500) -> Iterator[Tuple[str, Document]]:
        """Every document in id order."""
        after = None
        while True:
            rows = self.list(page, after)
            yield from rows
            if len(rows) < page:
                return
            after = rows[-1][0]

    def delete(self, id: str) -> bool:
        """Deletes the document. Returns whether it existed."""
        return self._db._native.delete(self.name, id)


class _Changes:
    """Iterator over a change subscription (see ``Database.changes``)."""

    def __init__(self, stream: "_native.Changes", timeout: Optional[float]) -> None:
        self._stream = stream
        self._timeout = timeout

    def __iter__(self) -> "_Changes":
        return self

    def __next__(self) -> Change:
        c = self._stream.next(self._timeout)
        if c is None:
            raise StopIteration
        return Change(c)


class Database:
    """A local database that syncs through a remote. Open it with :meth:`open`."""

    def __init__(self, native: "_native.Database") -> None:
        self._native = native

    @classmethod
    def open(
        cls,
        path: str,
        app_id: str,
        remote: "_native.Remote",
        *,
        encrypt: bool = False,
        password: Optional[str] = None,
        key: Optional[bytes] = None,
        device_name: Optional[str] = None,
        conflict_log_limit: Optional[int] = 1000,
        p2p: bool = False,
        stun: Optional[List[str]] = None,
        p2p_host: Optional[str] = None,
    ) -> "Database":
        """Opens (or creates) the local database at ``path``.

        For an encrypted database pass ``password`` (or a ``key`` from :meth:`export_key`); with
        ``encrypt=True`` a database this device creates is encrypted with ``password``. Without
        them an encrypted database opens locked: local reads and writes work, :meth:`sync`
        raises :class:`KeyRequiredError` until :meth:`unlock`. ``conflict_log_limit=None``
        turns the conflict log off.

        With ``p2p=True``, while :meth:`start_auto_sync` runs, the device opens direct channels
        to other online devices of the database, so their writes arrive within about a second.
        ``stun`` lists STUN servers (``host:port``) for peers outside the local network; the
        default is Google's public server, and ``[]`` keeps P2P on the local network.
        ``p2p_host`` is the local address to offer to peers instead of the detected one.
        """
        return cls(
            _native.Database.open(
                str(path),
                app_id,
                remote,
                encrypt=encrypt,
                password=password,
                key=None if key is None else bytes(key),
                device_name=device_name,
                conflict_log_limit=conflict_log_limit,
                p2p=p2p,
                stun=None if stun is None else list(stun),
                p2p_host=p2p_host,
            )
        )

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    @property
    def device_id(self) -> str:
        return self._native.device_id

    def db_id(self) -> Optional[str]:
        """The remote database id, or ``None`` before :meth:`init_or_join`."""
        return self._native.db_id()

    def init(self) -> str:
        """Creates a new remote database. Returns its id."""
        return self._native.init()

    def join(self, db_id: str) -> Dict[str, Any]:
        """Attaches to an existing remote database and pulls it."""
        return self._native.join(db_id)

    def init_or_join(self) -> str:
        """Attaches to the oldest database of this app on the remote, or creates one."""
        return self._native.init_or_join()

    def collection(self, name: str) -> Collection:
        """A collection. Names match ``[a-z0-9_]{1,64}``."""
        return Collection(self, name)

    def collections(self) -> List[str]:
        return self._native.collections()

    def sync(self) -> Dict[str, Any]:
        """Pushes local changes and pulls remote ones. Returns a report dict."""
        return self._native.sync()

    def changes(self, timeout: Optional[float] = None) -> Iterator[Change]:
        """Changes made from this call on, as they happen. Stops after ``timeout`` seconds
        without one. The subscription starts here, not at the first ``next()``, so a
        ``sync()`` between the two is not missed."""
        return _Changes(self._native.changes(), timeout)

    def start_auto_sync(self, debounce: float = 2.0, poll_interval: float = 300.0) -> None:
        """Syncs in the background: ``debounce`` seconds after local writes, and every
        ``poll_interval`` seconds."""
        self._native.start_auto_sync(debounce, poll_interval)

    def auto_sync_status(self) -> Union[None, Dict[str, Any], BothyError]:
        """The latest background outcome: ``None``, a report, or the error."""
        return self._native.auto_sync_status()

    def stop_auto_sync(self) -> None:
        self._native.stop_auto_sync()

    def p2p_status(self) -> Dict[str, Any]:
        """Direct channels: ``running``, ``discovering`` and ``peers`` (each a ``device_id``
        and a ``state``: ``signaling``, ``connected``, ``failed`` or ``disconnected``)."""
        return self._native.p2p_status()

    def status(self) -> Dict[str, Any]:
        """Local counters and the lock state."""
        return self._native.status()

    def conflicts(self, since_ms: Optional[int] = None) -> List[Dict[str, Any]]:
        """Writes from other devices that lost to a newer write, newest first."""
        return self._native.conflicts(since_ms)

    def clear_conflicts(self) -> int:
        return self._native.clear_conflicts()

    def is_encrypted(self) -> bool:
        return self._native.is_encrypted()

    def is_locked(self) -> bool:
        return self._native.is_locked()

    def unlock(self, password: str) -> None:
        """Raises :class:`WrongPasswordError` for a wrong password."""
        self._native.unlock(password)

    def unlock_with_key(self, key: bytes) -> None:
        self._native.unlock_with_key(bytes(key))

    def lock(self) -> None:
        self._native.lock()

    def export_key(self) -> bytes:
        """The 32-byte data key, to keep somewhere safe instead of the password."""
        return self._native.export_key()

    def change_password(self, old: str, new: str) -> None:
        self._native.change_password(old, new)

    def close(self) -> None:
        """Stops background sync and closes the database."""
        self._native.close()
