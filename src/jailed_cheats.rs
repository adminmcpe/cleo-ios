//! Jailbreak-free access to GTA:SA's built-in cheat table.
//!
//! CLEO 2.6 already contains the correct cheat indices for this game build.
//! We avoid hooking CCheat::DoCheats; actions are queued from the UIKit menu
//! and executed after the overlay closes on the main thread.

use once_cell::sync::Lazy;
use std::sync::Mutex;

const CHEAT_FUNCTION_TABLE: usize = 0x10065c358;
const CHEAT_ACTIVE_FLAGS: usize = 0x10072dda8;

extern "C" {
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
}

fn game_slide() -> usize {
    static SLIDE: Lazy<usize> = Lazy::new(|| unsafe {
        let a = _dyld_get_image_vmaddr_slide(0).max(0) as usize;
        let b = _dyld_get_image_vmaddr_slide(1).max(0) as usize;
        a.min(b)
    });
    *SLIDE
}

fn absolute(address: usize) -> usize {
    address + game_slide()
}

static NAMED_CHEATS: &[(usize, &str)] = &[
    (0, "THUGSARMOURY"),
    (1, "PROFESSIONALSKIT"),
    (2, "NUTTERSTOYS"),
    (10, "INEEDSOMEHELP"),
    (11, "TURNUPTHEHEAT"),
    (12, "TURNDOWNTHEHEAT"),
    (13, "PLEASANTLYWARM"),
    (14, "TOODAMNHOT"),
    (15, "DULLDULLDAY"),
    (16, "STAYINANDWATCHTV"),
    (17, "CANTSEEWHEREIMGOING"),
    (18, "TIMEJUSTFLIESBY"),
    (19, "SPEEDITUP"),
    (20, "SLOWITDOWN"),
    (21, "ROUGHNEIGHBOURHOOD"),
    (22, "STOPPICKINGONME"),
    (23, "SURROUNDEDBYNUTTERS"),
    (24, "TIMETOKICKASS"),
    (25, "OLDSPEEDDEMON"),
    (27, "NOTFORPUBLICROADS"),
    (28, "JUSTTRYANDSTOPME"),
    (29, "WHERESTHEFUNERAL"),
    (30, "CELEBRITYSTATUS"),
    (31, "TRUEGRIME"),
    (32, "18HOLES"),
    (33, "ALLCARSGOBOOM"),
    (34, "WHEELSONLYPLEASE"),
    (35, "STICKLIKEGLUE"),
    (36, "GOODBYECRUELWORLD"),
    (37, "DONTTRYANDSTOPME"),
    (38, "ALLDRIVERSARECRIMINALS"),
    (39, "PINKISTHENEWCOOL"),
    (40, "SOLONGASITSBLACK"),
    (42, "FLYINGFISH"),
    (43, "WHOATEALLTHEPIES"),
    (44, "BUFFMEUP"),
    (46, "LEANANDMEAN"),
    (47, "BLUESUEDESHOES"),
    (48, "ATTACKOFTHEVILLAGEPEOPLE"),
    (49, "LIFESABEACH"),
    (50, "ONLYHOMIESALLOWED"),
    (51, "BETTERSTAYINDOORS"),
    (52, "NINJATOWN"),
    (53, "LOVECONQUERSALL"),
    (54, "EVERYONEISPOOR"),
    (55, "EVERYONEISRICH"),
    (56, "CHITTYCHITTYBANGBANG"),
    (57, "CJPHONEHOME"),
    (58, "JUMPJET"),
    (59, "IWANTTOHOVER"),
    (60, "TOUCHMYCARYOUDIE"),
    (61, "SPEEDFREAK"),
    (62, "BUBBLECARS"),
    (63, "NIGHTPROWLER"),
    (64, "DONTBRINGONTHENIGHT"),
    (65, "SCOTTISHSUMMER"),
    (66, "SANDINMYEARS"),
    (68, "KANGAROO"),
    (69, "NOONECANHURTME"),
    (70, "MANFROMATLANTIS"),
    (71, "LETSGOBASEJUMPING"),
    (72, "ROCKETMAN"),
    (73, "IDOASIPLEASE"),
    (74, "BRINGITON"),
    (75, "STINGLIKEABEE"),
    (76, "IAMNEVERHUNGRY"),
    (77, "STATEOFEMERGENCY"),
    (78, "CRAZYTOWN"),
    (79, "TAKEACHILLPILL"),
    (80, "FULLCLIP"),
    (81, "IWANNADRIVEBY"),
    (82, "GHOSTTOWN"),
    (83, "HICKSVILLE"),
    (84, "WANNABEINMYGANG"),
    (85, "NOONECANSTOPUS"),
    (86, "ROCKETMAYHEM"),
    (87, "WORSHIPME"),
    (88, "HELLOLADIES"),
    (89, "ICANGOALLNIGHT"),
    (90, "PROFESSIONALKILLER"),
    (91, "NATURALTALENT"),
    (92, "OHDUDE"),
    (93, "FOURWHEELFUN"),
    (94, "HITTHEROADJACK"),
    (95, "ITSALLBULL"),
    (96, "FLYINGTOSTUNT"),
    (97, "MONSTERMASH"),
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

fn active(index: usize) -> bool {
    unsafe { *(absolute(CHEAT_ACTIVE_FLAGS + index) as *const bool) }
}

fn set_active(index: usize, value: bool) {
    unsafe {
        *(absolute(CHEAT_ACTIVE_FLAGS + index) as *mut bool) = value;
    }
}

fn function_address(index: usize) -> usize {
    unsafe {
        let slot = absolute(CHEAT_FUNCTION_TABLE + index * 8) as *const usize;
        slot.read()
    }
}

fn run(index: usize) {
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

    let mut out: Vec<CheatStatus> = NAMED_CHEATS
        .iter()
        .map(|(index, code)| {
            let is_active = active(*index);
            let is_queued = queued.contains(index);

            CheatStatus {
                table_index: *index,
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
        run(index);
    }
}

pub fn clear_queue() {
    QUEUED.lock().unwrap().clear();
}
