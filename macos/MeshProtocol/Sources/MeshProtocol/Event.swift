import Foundation
import P256K

/// A Nostr event — the unit of truth in Silent Mesh.
///
/// Everything the workspace knows is an event: messages, channel metadata,
/// work-thread commands, the relay's own notices. The client never trusts a
/// relay's summary of state; it folds signed events. So this type, and the
/// two operations on it (compute the id, verify the signature), are the
/// foundation the rest of the client stands on — if they are wrong, every
/// higher layer is confidently wrong.
public struct MeshEvent: Codable, Equatable, Sendable {
    /// 32-byte event id, lowercase hex — the SHA-256 of the canonical form.
    public var id: String
    /// 32-byte x-only author public key, lowercase hex.
    public var pubkey: String
    /// Unix seconds.
    public var createdAt: Int64
    /// Event kind (see ``MeshKind``).
    public var kind: Int
    /// Tag rows. Position 0 is the tag name; the rest are values.
    public var tags: [[String]]
    /// Free-form content; its meaning is kind-specific.
    public var content: String
    /// 64-byte BIP-340 signature, lowercase hex.
    public var sig: String

    enum CodingKeys: String, CodingKey {
        case id, pubkey, kind, tags, content, sig
        case createdAt = "created_at"
    }

    public init(
        id: String = "",
        pubkey: String,
        createdAt: Int64,
        kind: Int,
        tags: [[String]],
        content: String,
        sig: String = ""
    ) {
        self.id = id
        self.pubkey = pubkey
        self.createdAt = createdAt
        self.kind = kind
        self.tags = tags
        self.content = content
        self.sig = sig
    }

    /// The NIP-01 canonical serialization the id is taken over:
    /// `[0, pubkey, created_at, kind, tags, content]`.
    ///
    /// Written by hand rather than through `JSONEncoder` because the id is
    /// a hash: key order, whitespace, and escaping are all load-bearing, and
    /// an encoder that "helpfully" sorts keys or pretty-prints silently
    /// produces a different — and unverifiable — event.
    public func canonicalSerialization() throws -> Data {
        let array: [Any] = [0, pubkey, createdAt, kind, tags, content]
        return try JSONSerialization.data(withJSONObject: array, options: [.withoutEscapingSlashes])
    }

    /// The id this event's contents imply, independent of the `id` field.
    public func computedID() throws -> String {
        MeshHex.encode(SHA256Digest.hash(try canonicalSerialization()))
    }

    /// Is this event internally consistent — id matches its contents, and
    /// the signature is the author's over that id?
    ///
    /// Both halves matter and are commonly confused: a valid signature over
    /// a *different* id would let a relay swap an event's content while
    /// keeping a real signature, so the id is recomputed rather than
    /// trusted.
    public func isValid() -> Bool {
        guard let claimed = try? computedID(), claimed == id else { return false }
        guard var idBytes = MeshHex.decode(id), idBytes.count == 32,
              let pubkeyBytes = MeshHex.decode(pubkey), pubkeyBytes.count == 32,
              let sigBytes = MeshHex.decode(sig), sigBytes.count == 64,
              let signature = try? P256K.Schnorr.SchnorrSignature(dataRepresentation: sigBytes)
        else { return false }
        let xonly = P256K.Schnorr.XonlyKey(dataRepresentation: pubkeyBytes)
        return xonly.isValid(signature, for: &idBytes)
    }

    /// Sign this event with `keys`, filling in `pubkey`, `id`, and `sig`.
    public mutating func sign(with keys: MeshKeys) throws {
        pubkey = keys.publicKeyHex
        id = try computedID()
        guard var idBytes = MeshHex.decode(id) else {
            throw MeshProtocolError.malformed("event id is not hex")
        }
        sig = try keys.signSchnorr(&idBytes)
    }

    /// The first value of the first tag named `name`, if present.
    public func tagValue(_ name: String) -> String? {
        tags.first(where: { $0.first == name && $0.count > 1 })?[1]
    }

    /// Every value of every tag named `name`.
    public func tagValues(_ name: String) -> [String] {
        tags.filter { $0.first == name && $0.count > 1 }.map { $0[1] }
    }
}

/// Errors this layer raises. Deliberately small: a client that cannot tell
/// "the relay refused me" from "I built a bad event" cannot recover from
/// either.
public enum MeshProtocolError: Error, Equatable, Sendable {
    /// A value was not the shape the protocol requires.
    case malformed(String)
    /// The relay refused an operation, with its stated reason.
    case relay(String)
    /// The transport failed or closed unexpectedly.
    case transport(String)
    /// An operation did not complete in time.
    case timeout(String)
}

/// Lowercase-hex helpers. Nostr is hex end to end, and a client that
/// accepts uppercase in one place and emits it in another produces ids
/// that fail to match for reasons nobody can see.
public enum MeshHex {
    public static func encode<S: Sequence>(_ bytes: S) -> String where S.Element == UInt8 {
        bytes.map { String(format: "%02x", $0) }.joined()
    }

    public static func decode(_ hex: String) -> [UInt8]? {
        let chars = Array(hex)
        guard chars.count % 2 == 0 else { return nil }
        var out = [UInt8]()
        out.reserveCapacity(chars.count / 2)
        var i = 0
        while i < chars.count {
            guard let byte = UInt8(String(chars[i...i + 1]), radix: 16) else { return nil }
            out.append(byte)
            i += 2
        }
        return out
    }
}
