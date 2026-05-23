import Foundation
import IOBluetooth

/// Classic-Bluetooth connect/disconnect watcher for already-paired devices.
///
/// This stays in the macOS shell — `IOBluetooth` is Apple-only and the
/// equivalent on Windows is `Windows.Devices.Bluetooth`. The shell
/// normalizes events into `BTEvent`s and forwards them to the Rust core
/// over FFI; the core owns debounce (5 s default, configurable in M7)
/// and the session state machine.
struct PairedDevice: Identifiable, Hashable {
    let address: String
    let name: String
    var id: String { address }
}

enum BTEvent {
    case connected(address: String, name: String)
    case disconnected(address: String, name: String)
}

@MainActor
final class BluetoothWatcher: NSObject {
    private var connectObserver: IOBluetoothUserNotification?
    private var disconnectObservers: [String: IOBluetoothUserNotification] = [:]

    private var continuation: AsyncStream<BTEvent>.Continuation?
    let events: AsyncStream<BTEvent>

    var targetAddress: String?

    override init() {
        var cont: AsyncStream<BTEvent>.Continuation!
        self.events = AsyncStream { cont = $0 }
        super.init()
        self.continuation = cont
    }

    func start() {
        guard connectObserver == nil else { return }
        connectObserver = IOBluetoothDevice.register(
            forConnectNotifications: self,
            selector: #selector(handleConnect(_:device:))
        )
    }

    func stop() {
        connectObserver?.unregister()
        connectObserver = nil
        for obs in disconnectObservers.values { obs.unregister() }
        disconnectObservers.removeAll()
    }

    func pairedDevices() -> [PairedDevice] {
        let devices = IOBluetoothDevice.pairedDevices() as? [IOBluetoothDevice] ?? []
        return devices.compactMap { dev in
            guard let addr = dev.addressString else { return nil }
            return PairedDevice(address: normalize(addr), name: dev.name ?? addr)
        }
    }

    /// Currently-connected audio peripherals (speakers/headphones/headsets).
    /// The speaker picker filters to this so users don't have to scroll
    /// past keyboards, mice, or paired-but-offline devices.
    func connectedSpeakers() -> [PairedDevice] {
        let devices = IOBluetoothDevice.pairedDevices() as? [IOBluetoothDevice] ?? []
        return devices.compactMap { dev in
            guard dev.isConnected() else { return nil }
            guard dev.deviceClassMajor == UInt32(kBluetoothDeviceClassMajorAudio) else { return nil }
            guard let addr = dev.addressString else { return nil }
            return PairedDevice(address: normalize(addr), name: dev.name ?? addr)
        }
    }

    @objc private func handleConnect(_ notification: IOBluetoothUserNotification, device: IOBluetoothDevice) {
        guard let rawAddr = device.addressString else { return }
        let addr = normalize(rawAddr)
        let name = device.name ?? addr

        guard let target = targetAddress, normalize(target) == addr else { return }

        if disconnectObservers[addr] == nil {
            disconnectObservers[addr] = device.register(
                forDisconnectNotification: self,
                selector: #selector(handleDisconnect(_:device:))
            )
        }
        continuation?.yield(.connected(address: addr, name: name))
    }

    @objc private func handleDisconnect(_ notification: IOBluetoothUserNotification, device: IOBluetoothDevice) {
        guard let rawAddr = device.addressString else { return }
        let addr = normalize(rawAddr)
        let name = device.name ?? addr
        disconnectObservers.removeValue(forKey: addr)?.unregister()
        continuation?.yield(.disconnected(address: addr, name: name))
    }

    private func normalize(_ address: String) -> String {
        address.lowercased().replacingOccurrences(of: "-", with: ":")
    }
}
