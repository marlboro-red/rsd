// Summon: menu-bar presence + global ⌥Space hotkey (Carbon RegisterEventHotKey
// — no Accessibility permission needed). RSD lives in the menu bar; the
// palette appears over whatever you're doing and hides on Escape.

import AppKit
import Carbon
import SwiftUI

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
        item.button?.action = #selector(toggle)
        item.button?.target = self
        statusItem = item

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
