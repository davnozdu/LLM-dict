// Зонд перехватчика клавиатуры.
//
// Показывает, какие события клавиатуры вообще доходят до CGEventTap на этой
// машине. Нужен, чтобы отделить ошибку приложения от того, что события до него
// не доходят: в журнале LLM-dict видно только модификаторы, а обычные клавиши
// не появляются никогда.
//
// Запуск:  swift build/keyprobe.swift
// Терминалу понадобится «Универсальный доступ» (Системные настройки →
// Конфиденциальность и безопасность → Универсальный доступ).

import Cocoa
import Carbon

let trusted = AXIsProcessTrusted()
print("Универсальный доступ у этого процесса:", trusted ? "есть" : "НЕТ")
print("Secure Event Input:", IsSecureEventInputEnabled() ? "ВКЛЮЧЁН (обычные клавиши будут скрыты)" : "выключен")

guard trusted else {
    print("""

    Без «Универсального доступа» перехватчик не поднимется.
    Добавьте Терминал в Системные настройки → Конфиденциальность и
    безопасность → Универсальный доступ, затем запустите снова.
    """)
    exit(1)
}

var counts: [String: Int] = [:]
let mask = (1 << CGEventType.keyDown.rawValue)
         | (1 << CGEventType.keyUp.rawValue)
         | (1 << CGEventType.flagsChanged.rawValue)

let callback: CGEventTapCallBack = { _, type, event, _ in
    let code = event.getIntegerValueField(.keyboardEventKeycode)
    let name: String
    switch type {
    case .keyDown: name = "KeyDown"
    case .keyUp: name = "KeyUp"
    case .flagsChanged: name = "FlagsChanged"
    default: name = "прочее(\(type.rawValue))"
    }
    counts[name, default: 0] += 1
    print(String(format: "%-13s код=%-4d флаги=0x%08X", (name as NSString).utf8String!, code, event.flags.rawValue))
    return Unmanaged.passUnretained(event)
}

guard let tap = CGEvent.tapCreate(tap: .cgSessionEventTap,
                                  place: .headInsertEventTap,
                                  options: .defaultTap,
                                  eventsOfInterest: CGEventMask(mask),
                                  callback: callback,
                                  userInfo: nil) else {
    print("Не удалось создать перехватчик.")
    exit(1)
}

let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .commonModes)
CGEvent.tapEnable(tap: tap, enable: true)

print("\nПеречисляю события 12 секунд. Нажмите ⌥⌘C и любые буквы.\n")

// Через две секунды сами изображаем ⌥⌘C — зонд проверит себя даже если
// ничего не нажимать.
DispatchQueue.main.asyncAfter(deadline: .now() + 2) {
    print("--- изображаю ⌥⌘C ---")
    let src = CGEventSource(stateID: .combinedSessionState)
    for (code, down) in [(UInt16(58), true), (UInt16(55), true), (UInt16(8), true),
                         (UInt16(8), false), (UInt16(55), false), (UInt16(58), false)] {
        let e = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: down)
        if code == 8 { e?.flags = [.maskAlternate, .maskCommand] }
        e?.post(tap: .cghidEventTap)
        usleep(40_000)
    }
}

DispatchQueue.main.asyncAfter(deadline: .now() + 12) {
    print("\n--- итог ---")
    for (k, v) in counts.sorted(by: { $0.key < $1.key }) { print("\(k): \(v)") }
    if counts["KeyDown", default: 0] == 0 {
        print("\nНи одного KeyDown. Обычные клавиши до перехватчиков не доходят —")
        print("это уровень системы, а не приложения.")
    } else {
        print("\nKeyDown приходят нормально.")
    }
    exit(0)
}

CFRunLoopRun()
