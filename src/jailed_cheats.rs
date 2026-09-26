//! Jailbreak-free access to GTA:SA's built-in cheat table.
//!
//! CLEO 2.6 already identified the game's 111 cheat slots for this iOS build.
//! The jailed port calls the existing game functions directly; no executable-memory
//! hook is required.

use once_cell::sync::Lazy;
use std::sync::Mutex;

pub(crate) const CHEAT_FUNCTION_TABLE: usize = 0x10065c358;
pub(crate) const CHEAT_ACTIVE_FLAGS: usize = 0x10072dda8;
pub(crate) const CHEAT_COUNT: usize = 111;

pub(crate) fn absolute(address: usize) -> usize {
    address + crate::jailed_runtime::game_slide()
}

// Every cheat slot is exposed. Entries without an official keyboard code are
// given a descriptive internal name so they are still usable from the menu.
static ALL_CHEATS: [&str; CHEAT_COUNT] = [
    "THUGSARMOURY",
    "PROFESSIONALSKIT",
    "NUTTERSTOYS",
    "[Weapons Set 4]",
    "[Clock Forward]",
    "[Skip Mission]",
    "[Debug Mappings]",
    "[Full Invincibility]",
    "[Debug Tap To Target]",
    "[Debug Targeting]",
    "INEEDSOMEHELP",
    "TURNUPTHEHEAT",
    "TURNDOWNTHEHEAT",
    "PLEASANTLYWARM",
    "TOODAMNHOT",
    "DULLDULLDAY",
    "STAYINANDWATCHTV",
    "CANTSEEWHEREIMGOING",
    "TIMEJUSTFLIESBY",
    "SPEEDITUP",
    "SLOWITDOWN",
    "ROUGHNEIGHBOURHOOD",
    "STOPPICKINGONME",
    "SURROUNDEDBYNUTTERS",
    "TIMETOKICKASS",
    "OLDSPEEDDEMON",
    "[Tinted Rancher]",
    "NOTFORPUBLICROADS",
    "JUSTTRYANDSTOPME",
    "WHERESTHEFUNERAL",
    "CELEBRITYSTATUS",
    "TRUEGRIME",
    "18HOLES",
    "ALLCARSGOBOOM",
    "WHEELSONLYPLEASE",
    "STICKLIKEGLUE",
    "GOODBYECRUELWORLD",
    "DONTTRYANDSTOPME",
    "ALLDRIVERSARECRIMINALS",
    "PINKISTHENEWCOOL",
    "SOLONGASITSBLACK",
    "[Sideways Wheels]",
    "FLYINGFISH",
    "WHOATEALLTHEPIES",
    "BUFFMEUP",
    "[Max Gambling]",
    "LEANANDMEAN",
    "BLUESUEDESHOES",
    "ATTACKOFTHEVILLAGEPEOPLE",
    "LIFESABEACH",
    "ONLYHOMIESALLOWED",
    "BETTERSTAYINDOORS",
    "NINJATOWN",
    "LOVECONQUERSALL",
    "EVERYONEISPOOR",
    "EVERYONEISRICH",
    "CHITTYCHITTYBANGBANG",
    "CJPHONEHOME",
    "JUMPJET",
    "IWANTTOHOVER",
    "TOUCHMYCARYOUDIE",
    "SPEEDFREAK",
    "BUBBLECARS",
    "NIGHTPROWLER",
    "DONTBRINGONTHENIGHT",
    "SCOTTISHSUMMER",
    "SANDINMYEARS",
    "[Predator]",
    "KANGAROO",
    "NOONECANHURTME",
    "MANFROMATLANTIS",
    "LETSGOBASEJUMPING",
    "ROCKETMAN",
    "IDOASIPLEASE",
    "BRINGITON",
    "STINGLIKEABEE",
    "IAMNEVERHUNGRY",
    "STATEOFEMERGENCY",
    "CRAZYTOWN",
    "TAKEACHILLPILL",
    "FULLCLIP",
    "IWANNADRIVEBY",
    "GHOSTTOWN",
    "HICKSVILLE",
    "WANNABEINMYGANG",
    "NOONECANSTOPUS",
    "ROCKETMAYHEM",
    "WORSHIPME",
    "HELLOLADIES",
    "ICANGOALLNIGHT",
    "PROFESSIONALKILLER",
    "NATURALTALENT",
    "OHDUDE",
    "FOURWHEELFUN",
    "HITTHEROADJACK",
    "ITSALLBULL",
    "FLYINGTOSTUNT",
    "MONSTERMASH",
    "[Prostitutes Pay]",
    "[Cool Taxis]",
    "[Melee Slot]",
    "[Handgun Slot]",
    "[SMG Slot]",
    "[Shotgun Slot]",
    "[Assault Rifle Slot]",
    "[Long Rifle Slot]",
    "[Thrown Slot]",
    "[Heavy Slot]",
    "[Equipment Slot]",
    "[Other Slot]",
    "[Xbox Helper]",
];

static QUEUED: Lazy<Mutex<Vec<usize>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Debug, Clone)]
pub struct CheatStatus {
    pub table_index: usize,
    pub code: &'static str,
    pub active: bool,
    pub queued: bool,
    pub will_be_active: bool,
}

pub(crate) fn active(index: usize) -> bool {
    if index >= CHEAT_COUNT {
        return false;
    }
    unsafe { *(absolute(CHEAT_ACTIVE_FLAGS + index) as *const bool) }
}

pub(crate) fn set_active(index: usize, value: bool) {
    if index >= CHEAT_COUNT {
        return;
    }
    unsafe {
        *(absolute(CHEAT_ACTIVE_FLAGS + index) as *mut bool) = value;
    }
}

pub(crate) fn function_address(index: usize) -> usize {
    if index >= CHEAT_COUNT {
        return 0;
    }

    unsafe {
        let slot = absolute(CHEAT_FUNCTION_TABLE + index * 8) as *const usize;
        slot.read()
    }
}

pub(crate) fn run_index(index: usize) {
    if index >= CHEAT_COUNT {
        return;
    }

    let function = function_address(index);

    if function != 0 {
        let f: extern "C" fn() = unsafe { std::mem::transmute(function) };
        f();
    } else {
        set_active(index, !active(index));
    }
}

pub fn statuses() -> Vec<CheatStatus> {
    let queued = QUEUED.lock().unwrap();

    let mut out: Vec<CheatStatus> = ALL_CHEATS
        .iter()
        .enumerate()
        .map(|(index, code)| {
            let is_active = active(index);
            let is_queued = queued.contains(&index);

            CheatStatus {
                table_index: index,
                code,
                active: is_active,
                queued: is_queued,
                will_be_active: if is_queued { !is_active } else { is_active },
            }
        })
        .collect();

    out.sort_by_key(|x| x.code);
    out
}

pub fn toggle_queue(sorted_index: usize) -> bool {
    if !crate::jailed_runtime::is_in_game() {
        return false;
    }

    let statuses = statuses();
    let Some(item) = statuses.get(sorted_index) else {
        return false;
    };

    let mut queued = QUEUED.lock().unwrap();

    if let Some(pos) = queued.iter().position(|x| *x == item.table_index) {
        queued.remove(pos);
    } else {
        queued.push(item.table_index);
    }

    true
}

pub fn process_queue() {
    if !crate::jailed_runtime::is_in_game() {
        return;
    }

    let pending = {
        let mut queue = QUEUED.lock().unwrap();
        std::mem::take(&mut *queue)
    };

    for index in pending {
        run_index(index);
    }
}

pub fn clear_queue() {
    QUEUED.lock().unwrap().clear();
}
