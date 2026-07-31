import Foundation
import P256K
import CryptoKit

/// A Silent Mesh identity: a secp256k1 keypair whose x-only public key is
/// the member's (or agent's) pubkey everywhere in the protocol.
///
/// The private key lives in memory here. Phase 4's `MeshVault` is what
/// wraps it in the Secure Enclave and unlocks it with biometrics; this type
/// is deliberately the plain version so the protocol layer can be tested
/// without entitlements, and so the vault has one clear thing to replace.
public struct MeshKeys: Sendable {
    /// Raw 32-byte secret. Stored as bytes rather than as a
    /// `P256K.Schnorr.PrivateKey` because that type is not `Sendable`, and
    /// an identity has to cross task boundaries in any real client. The
    /// key object is rebuilt per signature, which costs nothing measurable
    /// and keeps this type honestly copyable.
    private let privateKeyBytes: [UInt8]

    /// 32-byte x-only public key, lowercase hex.
    public let publicKeyHex: String

    /// Build from a 64-char hex private key (the `nsec` in raw form).
    public init(privateKeyHex: String) throws {
        guard let bytes = MeshHex.decode(privateKeyHex), bytes.count == 32 else {
            throw MeshProtocolError.malformed("private key must be 32 bytes of hex")
        }
        guard let key = try? P256K.Schnorr.PrivateKey(dataRepresentation: bytes) else {
            throw MeshProtocolError.malformed("not a valid secp256k1 private key")
        }
        privateKeyBytes = bytes
        publicKeyHex = MeshHex.encode(key.xonly.bytes)
    }

    /// Generate a fresh identity.
    public init() throws {
        guard let key = try? P256K.Schnorr.PrivateKey() else {
            throw MeshProtocolError.malformed("key generation failed")
        }
        privateKeyBytes = Array(key.dataRepresentation)
        publicKeyHex = MeshHex.encode(key.xonly.bytes)
    }

    /// BIP-340 sign exactly these bytes, with no additional hashing.
    ///
    /// `strict: true` is the important argument: Nostr signs the 32-byte
    /// event id itself, and silently signing a shorter or longer message
    /// would produce signatures every other implementation rejects.
    public func signSchnorr(_ message: inout [UInt8]) throws -> String {
        guard let privateKey = try? P256K.Schnorr.PrivateKey(dataRepresentation: privateKeyBytes)
        else {
            throw MeshProtocolError.malformed("stored key is no longer valid")
        }
        guard let signature = try? privateKey.signature(
            message: &message,
            auxiliaryRand: nil,
            strict: true
        ) else {
            throw MeshProtocolError.malformed("schnorr signing failed")
        }
        return MeshHex.encode(signature.dataRepresentation)
    }
}

/// SHA-256 over `Data`, kept behind one name so the hash used for event ids
/// is obvious at every call site.
public enum SHA256Digest {
    public static func hash(_ data: Data) -> [UInt8] {
        Array(CryptoKit.SHA256.hash(data: data))
    }
}
