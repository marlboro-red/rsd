// Summon: menu-bar presence + global ⌥Space hotkey (Carbon RegisterEventHotKey
// — no Accessibility permission needed). RSD lives in the menu bar; the
// palette appears over whatever you're doing and hides on Escape.

import AppKit
import Carbon
import SwiftUI

@MainActor
final class Summoner: NSObject, NSApplicationDelegate {
    private var statusItem: NSStatusItem?
    private var hotKeyRef: EventHotKeyRef?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory) // menu bar app: no Dock icon

        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        item.button?.image = NSImage(
            systemSymbolName: "sparkle.magnifyingglass",
            accessibilityDescription: "RSD"
        )
        item.button?.action = #selector(statusClicked)
        item.button?.target = self
        item.button?.sendAction(on: [.leftMouseUp, .rightMouseUp])
        statusItem = item

        Notifier.shared.setUp()
        DaemonManager.shared.ensureRunning()
        registerHotKey()
        showPalette()
    }

    /// ⌥Space — Spotlight keeps ⌘Space; we take the key next door.
    private func registerHotKey() {
        var hotKeyID = EventHotKeyID(signature: OSType(0x5253_4421), id: 1) // 'RSD!'
        var eventType = EventTypeSpec(
            eventClass: OSType(kEventClassKeyboard),
            eventKind: UInt32(kEventHotKeyPressed)
        )
        InstallEventHandler(
            GetApplicationEventTarget(),
            { _, _, userData in
                Unmanaged<Summoner>.fromOpaque(userData!).takeUnretainedValue().toggle()
                return noErr
            },
            1,
            &eventType,
            Unmanaged.passUnretained(self).toOpaque(),
            nil
        )
        RegisterEventHotKey(
            UInt32(kVK_Space),
            UInt32(optionKey),
            hotKeyID,
            GetApplicationEventTarget(),
            0,
            &hotKeyRef
        )
    }

    func applicationWillTerminate(_ notification: Notification) {
        DaemonManager.shared.stop()
    }

    @objc private func statusClicked() {
        if NSApp.currentEvent?.type == .rightMouseUp {
            showMenu()
        } else {
            toggle()
        }
    }

    private func showMenu() {
        let menu = NSMenu()
        let alerts = AlertStore.shared.alerts
        if alerts.isEmpty {
            let item = NSMenuItem(title: "No standing alerts — ⌘S in the palette", action: nil, keyEquivalent: "")
            item.isEnabled = false
            menu.addItem(item)
        } else {
            for alert in alerts {
                let item = NSMenuItem(title: "≈ \(alert.query)", action: #selector(removeAlert(_:)), keyEquivalent: "")
                item.target = self
                item.representedObject = alert
                item.toolTip = "Click to stop watching"
                menu.addItem(item)
            }
        }
        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Quit RSD", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))
        statusItem?.menu = menu
        statusItem?.button?.performClick(nil)
        statusItem?.menu = nil
    }

    @objc private func removeAlert(_ sender: NSMenuItem) {
        if let alert = sender.representedObject as? SavedAlert {
            AlertStore.shared.remove(alert)
        }
    }

    @objc func toggle() {
        if NSApp.isActive, NSApp.windows.contains(where: { $0.isVisible && $0.canBecomeKey }) {
            NSApp.hide(nil)
        } else {
            showPalette()
        }
    }

    private func showPalette() {
        NSApp.activate(ignoringOtherApps: true)
        for window in NSApp.windows where window.canBecomeKey {
            window.center()
            window.makeKeyAndOrderFront(nil)
        }
    }
}
