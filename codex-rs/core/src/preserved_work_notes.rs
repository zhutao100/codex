pub(crate) const PRESERVED_WORK_NOTES_MESSAGE_PREFIX: &str = "Immediately before compaction, the previous model emitted the following preserved session work notes.\nThese notes are verbatim and are intended to prevent duplicate work and repeated dead ends:\n\n";

pub(crate) fn is_preserved_work_notes_message(message: &str) -> bool {
    message.starts_with(PRESERVED_WORK_NOTES_MESSAGE_PREFIX)
}
