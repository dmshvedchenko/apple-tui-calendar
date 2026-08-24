import CoreGraphics
import EventKit
import Foundation

let protocolVersion = 2

// These raw values are the v2 CalendarError wire contract shared with Rust.
// They are intentionally not localized and never expose NSError domains.
enum CalendarErrorCode: String {
    case notFound = "not_found"
    case permissionDenied = "permission_denied"
    case readOnly = "read_only"
    case cannotModifyMetadata = "cannot_modify_metadata"
    case cannotDelete = "cannot_delete"
    case sourceNotFound = "source_not_found"
    case sourceUnavailable = "source_unavailable"
    case unsupported = "unsupported"
    case invalidTitle = "invalid_title"
    case invalidColor = "invalid_color"
    case internalError = "internal"
}

struct CalendarFailure: Error {
    let code: CalendarErrorCode
    let message: String
}

struct ServiceFailure: Error {
    let code: String
    let message: String

    init(_ code: String, _ message: String) {
        self.code = code
        self.message = message
    }
}

/// Provider-normalized identity for an EventKit floating all-day interval.
/// These are calendar dates, not UTC instants.
struct AllDayDateRange: Equatable {
    let startDate: String
    let endDateExclusive: String
}

/// Normalizes both EventKit all-day end-date presentations to a half-open
/// calendar-date interval. Modern macOS exposes the final visible day at
/// 23:59:59; earlier EventKit releases exposed midnight of the following day.
/// The caller supplies the EventKit/default calendar context deliberately: a
/// floating event must never be converted through UTC before extracting dates.
func normalizedAllDayDateRange(start: Date, end: Date, calendar: Calendar) -> AllDayDateRange? {
    guard end > start else { return nil }

    let startBoundary = calendar.startOfDay(for: start)
    let endBoundary = calendar.startOfDay(for: end)
    let endTime = calendar.dateComponents([.hour, .minute, .second, .nanosecond], from: end)
    let isMidnight = endTime.hour == 0
        && endTime.minute == 0
        && endTime.second == 0
        && (endTime.nanosecond ?? 0) == 0
    guard let exclusiveBoundary = isMidnight
        ? Optional(endBoundary)
        : calendar.date(byAdding: .day, value: 1, to: endBoundary),
        exclusiveBoundary > startBoundary else {
        return nil
    }

    let formatter = DateFormatter()
    formatter.calendar = calendar
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = calendar.timeZone
    formatter.dateFormat = "yyyy-MM-dd"
    return AllDayDateRange(
        startDate: formatter.string(from: startBoundary),
        endDateExclusive: formatter.string(from: exclusiveBoundary)
    )
}

final class Output: @unchecked Sendable {
    private let lock = NSLock()

    func write(_ object: [String: Any]) {
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object),
              let line = String(data: data, encoding: .utf8) else { return }
        lock.lock()
        FileHandle.standardOutput.write(Data((line + "\n").utf8))
        lock.unlock()
    }
}

final class AccessResult: @unchecked Sendable {
    private let lock = NSLock()
    private var storedError: Error?

    func set(_ error: Error?) {
        lock.lock()
        storedError = error
        lock.unlock()
    }

    func get() -> Error? {
        lock.lock()
        defer { lock.unlock() }
        return storedError
    }
}

final class CalendarService: @unchecked Sendable {
    private let store = EKEventStore()
    private let output: Output
    private let iso = ISO8601DateFormatter()
    private let fractionalISO = ISO8601DateFormatter()

    init(output: Output) {
        self.output = output
        fractionalISO.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        NotificationCenter.default.addObserver(
            forName: .EKEventStoreChanged,
            object: store,
            queue: nil
        ) { [weak self] _ in
            self?.store.reset()
            self?.output.write(["protocol": protocolVersion, "notification": "storeChanged"])
        }
    }

    func handle(_ request: [String: Any]) -> [String: Any] {
        let id = request["id"] as? UInt64 ?? UInt64(request["id"] as? Int ?? 0)
        guard request["protocol"] as? Int == protocolVersion else {
            return failure(id, ServiceFailure("protocolMismatch", "expected IPC protocol v\(protocolVersion)"))
        }
        guard let method = request["method"] as? String else {
            return failure(id, ServiceFailure("invalid", "missing method"))
        }
        let params = request["params"] as? [String: Any] ?? [:]
        do {
            let result: Any
            switch method {
            case "authorizationStatus": result = authorizationStatus()
            case "requestAccess": result = try requestAccess()
            case "listCalendars":
                try requireReadAccess()
                result = store.calendars(for: .event).map(calendarJSON)
            case "calendar.capabilities":
                result = ["canListSources": true, "canCreate": true, "canUpdate": true, "canDelete": true, "canChangeColor": true]
            case "calendar.sources": result = try calendarSources()
            case "calendar.create": result = try createCalendar(params)
            case "calendar.rename": result = try renameCalendar(params)
            case "calendar.setColor": result = try setCalendarColor(params)
            case "calendar.delete": result = try deleteCalendar(params)
            case "fetchEvents": result = try fetchEvents(params)
            case "createEvent": result = try createEvent(params)
            case "updateEvent": result = try updateEvent(params)
            case "deleteEvent": result = try deleteEvent(params)
            case "respondInvitation":
                throw ServiceFailure("unsupported", "Apple's public EventKit API exposes invitation status as read-only; RSVP in Calendar.app")
            default: throw ServiceFailure("invalid", "unknown method: \(method)")
            }
            return ["protocol": protocolVersion, "id": id, "result": result]
        } catch let error as ServiceFailure {
            return failure(id, error)
        } catch let error as CalendarFailure {
            return failure(id, ServiceFailure(error.code.rawValue, error.message))
        } catch {
            return failure(id, ServiceFailure("service", error.localizedDescription))
        }
    }

    private func failure(_ id: UInt64, _ error: ServiceFailure) -> [String: Any] {
        ["protocol": protocolVersion, "id": id, "error": ["code": error.code, "message": error.message]]
    }

    private func authorizationStatus() -> String {
        switch EKEventStore.authorizationStatus(for: .event) {
        case .notDetermined: return "notDetermined"
        case .restricted: return "restricted"
        case .denied: return "denied"
        case .fullAccess: return "fullAccess"
        case .writeOnly: return "writeOnly"
        @unknown default: return "notDetermined"
        }
    }

    private func requestAccess() throws -> String {
        let semaphore = DispatchSemaphore(value: 0)
        let result = AccessResult()
        if #available(macOS 14.0, *) {
            store.requestFullAccessToEvents { _, error in
                result.set(error)
                semaphore.signal()
            }
        } else {
            store.requestAccess(to: .event) { _, error in
                result.set(error)
                semaphore.signal()
            }
        }
        let deadline = Date().addingTimeInterval(60)
        while semaphore.wait(timeout: .now()) == .timedOut {
            if Date() >= deadline {
                throw ServiceFailure("service", "calendar permission request timed out")
            }
            RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05))
        }
        if let accessError = result.get() { throw accessError }
        return authorizationStatus()
    }

    private func requireReadAccess() throws {
        let status = EKEventStore.authorizationStatus(for: .event)
        if status == .denied || status == .restricted { throw ServiceFailure("permissionDenied", "Calendar access is disabled in System Settings → Privacy & Security → Calendars") }
        if status == .notDetermined { throw ServiceFailure("permissionDenied", "Calendar permission has not been requested") }
        if #available(macOS 14.0, *), status == .writeOnly {
            throw ServiceFailure("permissionDenied", "Full Calendar access is required for browsing events")
        }
    }

    private func calendarSources() throws -> [[String: Any]] {
        do {
            try requireReadAccess()
        } catch let error as ServiceFailure {
            let code: CalendarErrorCode = error.code == "permissionDenied" ? .permissionDenied : .internalError
            throw CalendarFailure(code: code, message: code == .permissionDenied ? "Calendar access denied" : "Calendar source discovery failed")
        }
        return store.sources.map(sourceJSON)
    }

    private func createCalendar(_ params: [String: Any]) throws -> [String: Any] {
        try requireReadAccess()
        guard let input = params["calendar"] as? [String: Any],
              let title = input["title"] as? String,
              !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw CalendarFailure(code: .invalidTitle, message: "Calendar title is required")
        }
        guard let sourceID = input["sourceId"] as? String,
              let source = store.sources.first(where: { $0.sourceIdentifier == sourceID }) else {
            throw CalendarFailure(code: .sourceNotFound, message: "Calendar source was not found")
        }
        guard store.calendars(for: .event).contains(where: { $0.source.sourceIdentifier == source.sourceIdentifier && $0.allowsContentModifications }) else {
            throw CalendarFailure(code: .sourceUnavailable, message: "Calendar source does not allow calendar creation")
        }
        guard let colorText = input["color"] as? String, let color = parseCalendarColor(colorText) else {
            throw CalendarFailure(code: .invalidColor, message: "Color must use #RRGGBB")
        }
        let calendar = EKCalendar(for: .event, eventStore: store)
        calendar.source = source
        calendar.title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        calendar.cgColor = color
        do { try store.saveCalendar(calendar, commit: true) }
        catch { throw CalendarFailure(code: .cannotModifyMetadata, message: "Calendar could not be created") }
        return calendarJSON(calendar)
    }

    private func renameCalendar(_ params: [String: Any]) throws -> [String: Any] {
        do {
            try requireReadAccess()
        } catch {
            throw CalendarFailure(code: .permissionDenied, message: "Calendar access denied")
        }
        guard let input = params["calendar"] as? [String: Any],
              let calendarID = input["calendarId"] as? String,
              let calendar = store.calendar(withIdentifier: calendarID) else {
            throw CalendarFailure(code: .notFound, message: "Calendar was not found")
        }
        guard let title = input["title"] as? String,
              !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw CalendarFailure(code: .invalidTitle, message: "Calendar title is required")
        }
        // This is the public EventKit permission that governs calendar
        // modifications. It intentionally remains separate from deletion.
        guard calendar.allowsContentModifications else {
            throw CalendarFailure(code: .cannotModifyMetadata, message: "Calendar metadata cannot be modified")
        }
        calendar.title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            try store.saveCalendar(calendar, commit: true)
        } catch {
            throw CalendarFailure(code: .cannotModifyMetadata, message: "Calendar could not be renamed")
        }
        return calendarJSON(calendar)
    }

    private func setCalendarColor(_ params: [String: Any]) throws -> [String: Any] {
        do {
            try requireReadAccess()
        } catch {
            throw CalendarFailure(code: .permissionDenied, message: "Calendar access denied")
        }
        guard let input = params["calendar"] as? [String: Any],
              let calendarID = input["calendarId"] as? String,
              let calendar = store.calendar(withIdentifier: calendarID) else {
            throw CalendarFailure(code: .notFound, message: "Calendar was not found")
        }
        guard calendar.allowsContentModifications else {
            throw CalendarFailure(code: .cannotModifyMetadata, message: "Calendar metadata cannot be modified")
        }
        guard let colorText = input["color"] as? String, let color = parseCalendarColor(colorText) else {
            throw CalendarFailure(code: .invalidColor, message: "Color must use #RRGGBB")
        }
        calendar.cgColor = color
        do {
            try store.saveCalendar(calendar, commit: true)
        } catch {
            throw CalendarFailure(code: .cannotModifyMetadata, message: "Calendar color could not be changed")
        }
        return calendarJSON(calendar)
    }

    private func deleteCalendar(_ params: [String: Any]) throws -> [String: Any] {
        do {
            try requireReadAccess()
        } catch {
            throw CalendarFailure(code: .permissionDenied, message: "Calendar access denied")
        }
        guard let input = params["calendar"] as? [String: Any],
              let calendarID = input["calendarId"] as? String,
              let calendar = store.calendar(withIdentifier: calendarID) else {
            throw CalendarFailure(code: .notFound, message: "Calendar was not found")
        }
        guard supportsCalendarDeletion(calendar) else {
            throw CalendarFailure(code: .unsupported, message: "Calendar source does not support deletion")
        }
        guard calendarPermissions(calendar)["canDelete"] == true else {
            throw CalendarFailure(code: .cannotDelete, message: "Calendar cannot be deleted")
        }
        do {
            try store.removeCalendar(calendar, commit: true)
        } catch {
            throw CalendarFailure(code: .cannotDelete, message: "Calendar could not be deleted")
        }
        return ["calendarId": calendarID]
    }

    private func parseCalendarColor(_ value: String) -> CGColor? {
        guard value.count == 7, value.first == "#", let rgb = Int(value.dropFirst(), radix: 16) else { return nil }
        return CGColor(red: CGFloat((rgb >> 16) & 0xff) / 255, green: CGFloat((rgb >> 8) & 0xff) / 255, blue: CGFloat(rgb & 0xff) / 255, alpha: 1)
    }

    // EventKit does not provide an "is deletable" flag. Keep the public
    // contract conservative: only local calendars are eligible, and never
    // derive deletion permission from event-content writability.
    private func supportsCalendarDeletion(_ calendar: EKCalendar) -> Bool {
        calendar.source.sourceType == .local
    }

    private func calendarPermissions(_ calendar: EKCalendar) -> [String: Bool] {
        return [
            "canCreateEvents": calendar.allowsContentModifications,
            "canModifyEvents": calendar.allowsContentModifications,
            "canModifyMetadata": calendar.allowsContentModifications,
            "canDelete": supportsCalendarDeletion(calendar)
        ]
    }

    private func fetchEvents(_ params: [String: Any]) throws -> [[String: Any]] {
        try requireReadAccess()
        // Protocol v2 may also carry `fetchRequest.allDayRange`, a floating
        // calendar-date intent. This helper deliberately continues to use the
        // established UTC predicate until a separate EventKit date-predicate
        // design is adopted; it must not pretend the two representations are
        // interchangeable.
        guard let start = date(params["start"]), let end = date(params["end"]), start < end else {
            throw ServiceFailure("invalid", "fetchEvents requires a valid start and end")
        }
        let ids = params["calendarIds"] as? [String] ?? []
        let calendars = ids.isEmpty ? nil : ids.compactMap(store.calendar(withIdentifier:))
        let predicate = store.predicateForEvents(withStart: start, end: end, calendars: calendars)
        return store.events(matching: predicate).map(eventJSON)
    }

    private func createEvent(_ params: [String: Any]) throws -> [String: Any] {
        guard let input = params["event"] as? [String: Any] else {
            throw ServiceFailure("invalid", "missing event")
        }
        let event = EKEvent(eventStore: store)
        try apply(input, to: event, creating: true, alarmMutation: nil)
        try store.save(event, span: .thisEvent, commit: true)
        return eventJSON(event)
    }

    private func updateEvent(_ params: [String: Any]) throws -> [String: Any] {
        guard let input = params["event"] as? [String: Any], let id = input["id"] as? String else {
            throw ServiceFailure("invalid", "missing event id")
        }
        guard let event = store.event(withIdentifier: id) else {
            throw ServiceFailure("notFound", id)
        }
        try apply(input, to: event, creating: false, alarmMutation: params["alarmMutation"] as? [String: Any], timeMutation: params["timeMutation"] as? [String: Any])
        try store.save(event, span: span(params["span"]), commit: true)
        return eventJSON(event)
    }

    private func deleteEvent(_ params: [String: Any]) throws -> NSNull {
        guard let id = params["id"] as? String, let event = store.event(withIdentifier: id) else {
            throw ServiceFailure("notFound", params["id"] as? String ?? "missing event id")
        }
        try store.remove(event, span: span(params["span"]), commit: true)
        return NSNull()
    }

    private func applyCreateTime(_ time: [String: Any], to event: EKEvent) throws -> Bool {
        guard let kind = time["kind"] as? String else {
            throw ServiceFailure("invalid", "event time kind is required")
        }
        switch kind {
        case "timed":
            guard let start = date(time["start"]), let end = date(time["end"]), start < end else {
                throw ServiceFailure("invalid", "valid timed start and end are required")
            }
            event.startDate = start
            event.endDate = end
            event.isAllDay = false
            return true
        case "allDay":
            guard let startText = time["startDate"] as? String,
                  let endText = time["endDateExclusive"] as? String,
                  let dates = allDayDates(start: startText, endExclusive: endText) else {
                throw ServiceFailure("invalid", "valid all-day start and exclusive end dates are required")
            }
            event.startDate = dates.0
            event.endDate = dates.1
            event.isAllDay = true
            // `allDayDates` uses a calendar solely to construct floating
            // EventKit dates. Do not assign that construction zone to event.
            return false
        case "legacyAllDayUnknown":
            throw ServiceFailure("invalid", "legacy all-day create input is not supported")
        default:
            throw ServiceFailure("invalid", "unknown event time kind")
        }
    }

    private func apply(_ input: [String: Any], to event: EKEvent, creating: Bool, alarmMutation: [String: Any]?, timeMutation: [String: Any]? = nil) throws {
        guard let title = input["title"] as? String, !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              let calendarID = input["calendarId"] as? String,
              let calendar = store.calendar(withIdentifier: calendarID) else {
            throw ServiceFailure("invalid", "title and writable calendar are required")
        }
        guard calendar.allowsContentModifications else {
            throw ServiceFailure("invalid", "calendar is read-only")
        }
        let requestedAttendees = input["attendees"] as? [String] ?? []
        if !requestedAttendees.isEmpty {
            throw ServiceFailure("unsupported", "Apple's public EventKit API does not permit adding attendees; create the event first and invite attendees in Calendar.app")
        }
        event.title = title
        event.calendar = calendar
        let timeKind = timeMutation?["kind"] as? String ?? "replaceLegacy"
        let assignTimeZone: Bool
        if creating {
            guard let time = input["time"] as? [String: Any] else {
                throw ServiceFailure("invalid", "typed event time is required")
            }
            assignTimeZone = try applyCreateTime(time, to: event)
        } else if timeKind == "replaceLegacy" {
            guard let start = date(input["start"]), let end = date(input["end"]), start < end else {
                throw ServiceFailure("invalid", "valid event start and end are required")
            }
            event.startDate = start
            event.endDate = end
            event.isAllDay = input["allDay"] as? Bool ?? false
            assignTimeZone = true
        } else if timeKind == "replaceAllDay" {
            guard let startText = timeMutation?["startDate"] as? String,
                  let endText = timeMutation?["endDateExclusive"] as? String,
                  let dates = allDayDates(start: startText, endExclusive: endText) else {
                throw ServiceFailure("invalid", "invalid all-day date range")
            }
            event.startDate = dates.0
            event.endDate = dates.1
            event.isAllDay = true
            assignTimeZone = false
        } else if timeKind != "preserve" {
            throw ServiceFailure("invalid", "unknown time mutation")
        } else {
            assignTimeZone = false
        }
        event.location = input["location"] as? String
        event.notes = input["notes"] as? String
        event.url = URL(string: input["url"] as? String ?? "")
        if assignTimeZone, let zone = input["timeZone"] as? String, let timeZone = TimeZone(identifier: zone) {
            event.timeZone = timeZone
        }
        event.availability = availability(input["availability"] as? String)
        if creating {
            event.alarms = try replacementAlarms(input["alarms"])
        } else if let alarmMutation {
            guard let kind = alarmMutation["kind"] as? String else {
                throw ServiceFailure("invalidAlarm", "alarm mutation intent is invalid")
            }
            switch kind {
            case "preserve": break
            case "replace": event.alarms = try replacementAlarms(alarmMutation["alarms"])
            default: throw ServiceFailure("invalidAlarm", "unknown alarm mutation intent")
            }
        }
        if let rules = input["recurrence"] as? [[String: Any]] {
            event.recurrenceRules = try rules.map(recurrenceRule)
        } else if creating {
            event.recurrenceRules = []
        }
    }

    private func allDayDates(start: String, endExclusive: String) -> (Date, Date)? {
        let formatter = DateFormatter()
        formatter.calendar = Calendar.current
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = Calendar.current.timeZone
        formatter.dateFormat = "yyyy-MM-dd"
        guard let startDate = formatter.date(from: start), let endDate = formatter.date(from: endExclusive), endDate > startDate else { return nil }
        return (startDate, endDate)
    }

    private func calendarJSON(_ calendar: EKCalendar) -> [String: Any] {
        [
            "id": calendar.calendarIdentifier,
            "sourceId": calendar.source.sourceIdentifier,
            "title": calendar.title,
            "account": calendar.source.title,
            "provider": provider(calendar.source.sourceType),
            "color": hex(calendar.cgColor),
            "isWritable": calendar.allowsContentModifications,
            "permissions": calendarPermissions(calendar),
            "enabled": true
        ]
    }

    private func sourceJSON(_ source: EKSource) -> [String: Any] {
        let writable = store.calendars(for: .event).contains { $0.source.sourceIdentifier == source.sourceIdentifier && $0.allowsContentModifications }
        let kind: String
        switch source.sourceType {
        case .local: kind = "local"
        case .exchange: kind = "exchange"
        case .calDAV: kind = "caldav"
        case .mobileMe: kind = "icloud"
        case .subscribed: kind = "subscribed"
        case .birthdays: kind = "birthdays"
        @unknown default: kind = "other"
        }
        return ["id": source.sourceIdentifier, "title": source.title, "sourceType": kind, "isWritable": writable]
    }

    private func eventJSON(_ event: EKEvent) -> [String: Any] {
        let attendees = (event.attendees ?? []).map(participantJSON)
        var result: [String: Any] = [
            "id": event.eventIdentifier ?? event.calendarItemIdentifier,
            "calendarId": event.calendar.calendarIdentifier,
            "title": event.title ?? "(Untitled)",
            "start": iso.string(from: event.startDate),
            "end": iso.string(from: event.endDate),
            "allDay": event.isAllDay,
            "location": event.location ?? "",
            "notes": event.notes ?? "",
            "url": event.url?.absoluteString ?? "",
            "timeZone": event.timeZone?.identifier ?? TimeZone.current.identifier,
            "timeZoneProvenance": event.timeZone == nil ? "helperFallback" : "explicitEvent",
            "availability": availabilityName(event.availability),
            "attendees": attendees,
            "alarms": (event.alarms ?? []).map(alarmJSON),
            "recurrence": (event.recurrenceRules ?? []).map(recurrenceJSON),
            "hasRecurrence": event.hasRecurrenceRules,
            "isDetached": event.isDetached,
            "invitationStatus": attendees.first(where: { ($0["isCurrentUser"] as? Bool) == true })?["status"] as? String ?? "unknown"
        ]
        if event.isAllDay,
           let dates = normalizedAllDayDateRange(
               start: event.startDate,
               end: event.endDate,
               calendar: Calendar.current
           ) {
            result["allDayStartDate"] = dates.startDate
            result["allDayEndDateExclusive"] = dates.endDateExclusive
        }
        if let organizer = event.organizer { result["organizer"] = participantJSON(organizer) }
        return result
    }

    private func participantJSON(_ participant: EKParticipant) -> [String: Any] {
        let address = participant.url.absoluteString
        return [
            "name": participant.name ?? "",
            "email": address.hasPrefix("mailto:") ? String(address.dropFirst(7)) : address,
            "status": participantStatus(participant.participantStatus),
            "isCurrentUser": participant.isCurrentUser,
            "role": participantRole(participant.participantRole),
            "participantType": participantType(participant.participantType),
        // EKParticipant does not expose delivery scheduling status through its
        // public macOS API. Keep this explicit so clients do not imply it is
        // known or editable.
        "scheduleStatus": "unavailable"
        ]
    }

    private func alarmJSON(_ alarm: EKAlarm) -> [String: Any] {
        let isAbsolute = alarm.absoluteDate != nil
        let integralOffset = alarm.relativeOffset.rounded() == alarm.relativeOffset
        return [
            "relativeSeconds": alarm.absoluteDate == nil ? Int64(alarm.relativeOffset) as Any : NSNull(),
            "absoluteDate": alarm.absoluteDate.map(iso.string) ?? NSNull(),
            "isEditable": alarm.structuredLocation == nil && alarm.proximity == .none && (isAbsolute || integralOffset)
        ]
    }

    private func replacementAlarms(_ value: Any?) throws -> [EKAlarm] {
        guard let inputs = value as? [[String: Any]] else {
            throw ServiceFailure("invalidAlarm", "alarm replacement must contain an array")
        }
        return try inputs.map(alarm)
    }

    private func alarm(_ input: [String: Any]) throws -> EKAlarm {
        let absolute = date(input["absoluteDate"])
        let relative = input["relativeSeconds"] as? NSNumber
        switch (absolute, relative) {
        case let (date?, nil): return EKAlarm(absoluteDate: date)
        case let (nil, seconds?): return EKAlarm(relativeOffset: seconds.doubleValue)
        default: throw ServiceFailure("invalidAlarm", "alarm must contain exactly one of relativeSeconds or absoluteDate")
        }
    }

    private func recurrenceJSON(_ rule: EKRecurrenceRule) -> [String: Any] {
        var output: [String: Any] = [
            "frequency": frequencyName(rule.frequency),
            "interval": rule.interval,
            "daysOfWeek": (rule.daysOfTheWeek ?? []).map(dayName)
        ]
        if let end = rule.recurrenceEnd {
            output["occurrenceCount"] = end.occurrenceCount == 0 ? NSNull() : end.occurrenceCount
            output["endDate"] = end.endDate.map(iso.string) ?? NSNull()
        } else {
            output["occurrenceCount"] = NSNull()
            output["endDate"] = NSNull()
        }
        return output
    }

    private func recurrenceRule(_ input: [String: Any]) throws -> EKRecurrenceRule {
        guard let frequencyText = input["frequency"] as? String,
              let frequency = frequency(frequencyText) else {
            throw ServiceFailure("invalid", "invalid recurrence frequency")
        }
        let interval = max(1, (input["interval"] as? NSNumber)?.intValue ?? 1)
        let end: EKRecurrenceEnd?
        if let count = input["occurrenceCount"] as? NSNumber, count.intValue > 0 {
            end = EKRecurrenceEnd(occurrenceCount: count.intValue)
        } else if let date = date(input["endDate"]) {
            end = EKRecurrenceEnd(end: date)
        } else { end = nil }
        let days = (input["daysOfWeek"] as? [String] ?? []).compactMap(recurrenceDay)
        if days.isEmpty {
            return EKRecurrenceRule(recurrenceWith: frequency, interval: interval, end: end)
        }
        return EKRecurrenceRule(recurrenceWith: frequency, interval: interval,
            daysOfTheWeek: days, daysOfTheMonth: nil, monthsOfTheYear: nil,
            weeksOfTheYear: nil, daysOfTheYear: nil, setPositions: nil, end: end)
    }

    private func date(_ value: Any?) -> Date? {
        guard let text = value as? String else { return nil }
        return fractionalISO.date(from: text) ?? iso.date(from: text)
    }

    private func span(_ value: Any?) -> EKSpan {
        (value as? String) == "futureEvents" ? .futureEvents : .thisEvent
    }

    private func provider(_ type: EKSourceType) -> String {
        switch type {
        case .local: return "Local"
        case .exchange: return "Exchange"
        case .calDAV: return "CalDAV"
        case .mobileMe: return "iCloud"
        case .subscribed: return "Subscribed"
        case .birthdays: return "Birthdays"
        @unknown default: return "Unknown"
        }
    }

    private func participantStatus(_ status: EKParticipantStatus) -> String {
        switch status {
        case .accepted: return "accepted"
        case .declined: return "declined"
        case .tentative: return "tentative"
        case .pending: return "pending"
        case .delegated: return "delegated"
        default: return "unknown"
        }
    }

    private func participantRole(_ role: EKParticipantRole) -> String {
        switch role {
        case .required: return "required"
        case .optional: return "optional"
        case .chair: return "chair"
        case .nonParticipant: return "nonParticipant"
        default: return "unknown"
        }
    }

    private func participantType(_ type: EKParticipantType) -> String {
        switch type {
        case .person: return "person"
        case .room: return "room"
        case .resource: return "resource"
        case .group: return "group"
        default: return "unknown"
        }
    }

    private func availabilityName(_ availability: EKEventAvailability) -> String {
        switch availability {
        case .busy: return "busy"
        case .free: return "free"
        case .tentative: return "tentative"
        case .unavailable: return "unavailable"
        default: return "notSupported"
        }
    }

    private func availability(_ text: String?) -> EKEventAvailability {
        switch text {
        case "free": return .free
        case "tentative": return .tentative
        case "unavailable": return .unavailable
        default: return .busy
        }
    }

    private func frequencyName(_ frequency: EKRecurrenceFrequency) -> String {
        switch frequency {
        case .daily: return "daily"
        case .weekly: return "weekly"
        case .monthly: return "monthly"
        case .yearly: return "yearly"
        @unknown default: return "weekly"
        }
    }

    private func frequency(_ text: String) -> EKRecurrenceFrequency? {
        switch text.lowercased() {
        case "daily": return .daily
        case "weekly": return .weekly
        case "monthly": return .monthly
        case "yearly": return .yearly
        default: return nil
        }
    }

    private func dayName(_ day: EKRecurrenceDayOfWeek) -> String {
        let names: [EKWeekday: String] = [.monday: "MO", .tuesday: "TU", .wednesday: "WE",
            .thursday: "TH", .friday: "FR", .saturday: "SA", .sunday: "SU"]
        let prefix = day.weekNumber == 0 ? "" : String(day.weekNumber)
        return prefix + (names[day.dayOfTheWeek] ?? "MO")
    }

    private func recurrenceDay(_ text: String) -> EKRecurrenceDayOfWeek? {
        let upper = text.uppercased()
        let suffix = String(upper.suffix(2))
        let weekday: EKWeekday
        switch suffix {
        case "MO": weekday = .monday
        case "TU": weekday = .tuesday
        case "WE": weekday = .wednesday
        case "TH": weekday = .thursday
        case "FR": weekday = .friday
        case "SA": weekday = .saturday
        case "SU": weekday = .sunday
        default: return nil
        }
        let number = Int(upper.dropLast(2)) ?? 0
        return EKRecurrenceDayOfWeek(weekday, weekNumber: number)
    }

    private func hex(_ color: CGColor) -> String {
        guard let components = color.converted(to: CGColorSpace(name: CGColorSpace.sRGB)!,
                                                intent: .defaultIntent, options: nil)?.components else {
            return "#5E9EFF"
        }
        let red = Int((components[safe: 0] ?? 0.37) * 255)
        let green = Int((components[safe: 1] ?? 0.62) * 255)
        let blue = Int((components[safe: 2] ?? 1.0) * 255)
        return String(format: "#%02X%02X%02X", red, green, blue)
    }
}

extension Array {
    subscript(safe index: Int) -> Element? { indices.contains(index) ? self[index] : nil }
}

let output = Output()
let service = CalendarService(output: output)
while let line = readLine() {
    guard let data = line.data(using: .utf8),
          let request = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
        output.write(["protocol": protocolVersion, "id": 0, "error": ["code": "invalid", "message": "malformed JSON"]])
        continue
    }
    output.write(service.handle(request))
}
