/** The version of the Bothy engine this package was built with. */
export function version(): string;

export type ErrorKind =
  | "Offline"
  | "AuthRequired"
  | "RateLimited"
  | "QuotaExceeded"
  | "RemoteInconsistent"
  | "FormatTooNew"
  | "SchemaTooNew"
  | "WrongPassword"
  | "KeyRequired"
  | "Corrupt"
  | "DatabaseLocked"
  | "InvalidInput"
  | "Internal";

/** Every error the package throws or rejects with. Messages never contain secrets. */
export class BothyError extends Error {
  readonly kind: ErrorKind;
  constructor(kind: ErrorKind, message: string);
}

/** A folder shared by the devices: a local directory, or one that another tool syncs. */
export class FolderRemote {
  constructor(path: string);
  readonly path: string;
}

export interface OpenOptions {
  /** Where the local database lives. */
  path: string;
  /** Your application's id, for example `com.example.notes`. */
  appId: string;
  remote: FolderRemote;
  deviceName?: string;
}

/** What a document field can hold. Integers beyond 2^53 are `bigint`, bytes are `Uint8Array`. */
export type Value =
  | null
  | boolean
  | number
  | bigint
  | string
  | Uint8Array
  | Value[]
  | { [key: string]: Value };

export type Document = { [key: string]: Value };

export class Collection {
  /** Writes a whole document; fields not in `doc` are removed. Resolves to whether anything changed. */
  put(id: string, doc: Document): Promise<boolean>;
  /** The document, or `null` if there is none. */
  get(id: string): Promise<Document | null>;
  /**
   * Sets the fields in `fields` and removes those named in `unset`, creating the document if
   * needed. Resolves to whether anything changed. Prefer it over read-then-`put`: merging is per field.
   */
  patch(id: string, fields: Document, unset?: string[]): Promise<boolean>;
  /** Deletes a document. Resolves to whether it existed. */
  delete(id: string): Promise<boolean>;
  /** Documents in id order. `limit` defaults to 100; `after` is an id to continue from. */
  list(options?: ListOptions | null): Promise<Array<{ id: string; doc: Document }>>;
}

export interface ListOptions {
  limit?: number;
  after?: string;
}

/** What one `sync()` did. */
export interface SyncReport {
  /** Local operations packed into new files. */
  pushedOps: number;
  segmentsUp: number;
  segmentsDown: number;
  /** Operations in the files that were downloaded and applied. */
  pulledOps: number;
  /** Documents whose visible content changed because of remote operations. */
  changedDocs: number;
  snapshotsDown: number;
  snapshotsUp: number;
  /** Another device changed the password of this encrypted database. */
  passwordChanged: boolean;
  /** Remote files that were skipped because they failed validation. */
  skipped: string[];
}

export class Database {
  collection(name: string): Collection;
  static open(options: OpenOptions): Promise<Database>;
  /** Names of the collections that hold at least one visible document. */
  collections(): Promise<string[]>;
  /** Creates the database on the remote or joins the one that is there; returns its id (hex). */
  initOrJoin(): Promise<string>;
  /** Pushes local changes and pulls remote ones. */
  sync(): Promise<SyncReport>;
  /** Waits for operations in flight, then releases the local database. Calling it again is fine. */
  close(): Promise<void>;
}
