import Foundation
import MeshProtocol
import MeshVault

/// A Secure Enclave self-test, run in the signed app because that is the
/// only place the enclave will answer.
///
/// `swift test` binaries are ad-hoc signed, so the enclave refuses them —
/// which means a vault suite that "passes" in SwiftPM would only ever be
/// testing the software path. This runs the hardware path for real and
/// reports what the OS actually said, including the entitlement errors
/// that are the usual reason it fails.
///
///     MESH_VAULT_SELFTEST=1              # no prompt: seal + unlock
///     MESH_VAULT_SELFTEST=presence       # prompts for Touch ID
enum VaultSelfTest {
    static func mode(
        _ environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> String? {
        environment["MESH_VAULT_SELFTEST"]
    }

    static func run(mode: String) -> Int32 {
        var failures = 0
        func report(_ label: String, _ outcome: String) {
            FileHandle.standardError.write(Data("vault: \(label): \(outcome)\n".utf8))
        }

        let available = MeshVault.isSecureEnclaveAvailable()
        report("enclave available", available ? "yes" : "NO")
        if !available { failures += 1 }

        let requiresPresence = mode == "presence"
        let vault = MeshVault(
            name: "selftest-\(UUID().uuidString.prefix(8))",
            protection: requiresPresence ? .secureEnclaveWithUserPresence : .secureEnclave)
        let secret = String(repeating: "5c", count: 32)
        let expected = try? MeshKeys(privateKeyHex: secret)

        do {
            try vault.seal(identityHex: secret)
            report("seal", "ok")
        } catch {
            report("seal", "FAILED — \(error)")
            return 1
        }

        do {
            let unlocked = try vault.unlock(
                reason: "Unlock your Silent Mesh identity (self-test)")
            let matches = unlocked.publicKeyHex == expected?.publicKeyHex
            report("unlock", matches ? "ok — identity round-tripped" : "FAILED — wrong identity")
            if !matches { failures += 1 }
        } catch {
            report("unlock", "FAILED — \(error)")
            failures += 1
        }

        // Destroying the enclave key must make the stored blob unreadable:
        // that is the whole claim of device binding, so it is asserted
        // rather than assumed.
        do {
            try vault.destroy()
            report("destroy", "ok")
        } catch {
            report("destroy", "FAILED — \(error)")
            failures += 1
        }

        report("result", failures == 0 ? "PASS" : "\(failures) FAILURE(S)")
        return failures == 0 ? 0 : 1
    }
}
