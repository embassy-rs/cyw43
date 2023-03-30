use core::cmp::{max, min};

use ch::driver::LinkState;
use embassy_net_driver_channel as ch;
use embassy_time::{Duration, Timer};

pub use crate::bus::SpiBusCyw43;
use crate::consts::*;
use crate::events::{Event, EventQueue};
use crate::fmt::Bytes;
use crate::ioctl::{IoctlState, IoctlType};
use crate::structs::*;
use crate::{countries, PowerManagementMode};

pub struct Control<'a> {
    state_ch: ch::StateRunner<'a>,
    event_sub: &'a EventQueue,
    ioctl_state: &'a IoctlState,
}

impl<'a> Control<'a> {
    pub(crate) fn new(state_ch: ch::StateRunner<'a>, event_sub: &'a EventQueue, ioctl_state: &'a IoctlState) -> Self {
        Self {
            state_ch,
            event_sub,
            ioctl_state,
        }
    }

    pub async fn init(&mut self, clm: &[u8]) {
        const CHUNK_SIZE: usize = 1024;

        info!("Downloading CLM...");

        let mut offs = 0;
        for chunk in clm.chunks(CHUNK_SIZE) {
            let mut flag = DOWNLOAD_FLAG_HANDLER_VER;
            if offs == 0 {
                flag |= DOWNLOAD_FLAG_BEGIN;
            }
            offs += chunk.len();
            if offs == clm.len() {
                flag |= DOWNLOAD_FLAG_END;
            }

            let header = DownloadHeader {
                flag,
                dload_type: DOWNLOAD_TYPE_CLM,
                len: chunk.len() as _,
                crc: 0,
            };
            let mut buf = [0; 8 + 12 + CHUNK_SIZE];
            buf[0..8].copy_from_slice(b"clmload\x00");
            buf[8..20].copy_from_slice(&header.to_bytes());
            buf[20..][..chunk.len()].copy_from_slice(&chunk);
            self.ioctl(IoctlType::Set, IOCTL_CMD_SET_VAR, 0, &mut buf[..8 + 12 + chunk.len()])
                .await;
        }

        // check clmload ok
        assert_eq!(self.get_iovar_u32("clmload_status").await, 0);

        info!("Configuring misc stuff...");

        // Disable tx gloming which transfers multiple packets in one request.
        // 'glom' is short for "conglomerate" which means "gather together into
        // a compact mass".
        self.set_iovar_u32("bus:txglom", 0).await;
        self.set_iovar_u32("apsta", 1).await;

        // read MAC addr.
        let mut mac_addr = [0; 6];
        assert_eq!(self.get_iovar("cur_etheraddr", &mut mac_addr).await, 6);
        info!("mac addr: {:02x}", Bytes(&mac_addr));

        let country = countries::WORLD_WIDE_XX;
        let country_info = CountryInfo {
            country_abbrev: [country.code[0], country.code[1], 0, 0],
            country_code: [country.code[0], country.code[1], 0, 0],
            rev: if country.rev == 0 { -1 } else { country.rev as _ },
        };
        self.set_iovar("country", &country_info.to_bytes()).await;

        // set country takes some time, next ioctls fail if we don't wait.
        Timer::after(Duration::from_millis(100)).await;

        // Set antenna to chip antenna
        self.ioctl_set_u32(IOCTL_CMD_ANTDIV, 0, 0).await;

        self.set_iovar_u32("bus:txglom", 0).await;
        Timer::after(Duration::from_millis(100)).await;
        //self.set_iovar_u32("apsta", 1).await; // this crashes, also we already did it before...??
        //Timer::after(Duration::from_millis(100)).await;
        self.set_iovar_u32("ampdu_ba_wsize", 8).await;
        Timer::after(Duration::from_millis(100)).await;
        self.set_iovar_u32("ampdu_mpdu", 4).await;
        Timer::after(Duration::from_millis(100)).await;
        //self.set_iovar_u32("ampdu_rx_factor", 0).await; // this crashes

        //Timer::after(Duration::from_millis(100)).await;

        // evts
        let mut evts = EventMask {
            iface: 0,
            events: [0xFF; 24],
        };

        // Disable spammy uninteresting events.
        evts.unset(Event::RADIO);
        evts.unset(Event::IF);
        evts.unset(Event::PROBREQ_MSG);
        evts.unset(Event::PROBREQ_MSG_RX);
        evts.unset(Event::PROBRESP_MSG);
        evts.unset(Event::PROBRESP_MSG);
        evts.unset(Event::ROAM);

        self.set_iovar("bsscfg:event_msgs", &evts.to_bytes()).await;

        Timer::after(Duration::from_millis(100)).await;

        // set wifi up
        self.ioctl(IoctlType::Set, IOCTL_CMD_UP, 0, &mut []).await;

        Timer::after(Duration::from_millis(100)).await;

        self.ioctl_set_u32(110, 0, 1).await; // SET_GMODE = auto
        self.ioctl_set_u32(142, 0, 0).await; // SET_BAND = any

        Timer::after(Duration::from_millis(100)).await;

        self.state_ch.set_ethernet_address(mac_addr);

        info!("INIT DONE");
    }

    pub async fn set_power_management(&mut self, mode: PowerManagementMode) {
        // power save mode
        let mode_num = mode.mode();
        if mode_num == 2 {
            self.set_iovar_u32("pm2_sleep_ret", mode.sleep_ret_ms() as u32).await;
            self.set_iovar_u32("bcn_li_bcn", mode.beacon_period() as u32).await;
            self.set_iovar_u32("bcn_li_dtim", mode.dtim_period() as u32).await;
            self.set_iovar_u32("assoc_listen", mode.assoc() as u32).await;
        }
        self.ioctl_set_u32(86, 0, mode_num).await;
    }

    pub async fn join_open(&mut self, ssid: &str) {
        self.set_iovar_u32("ampdu_ba_wsize", 8).await;

        self.ioctl_set_u32(134, 0, 0).await; // wsec = open
        self.set_iovar_u32x2("bsscfg:sup_wpa", 0, 0).await;
        self.ioctl_set_u32(20, 0, 1).await; // set_infra = 1
        self.ioctl_set_u32(22, 0, 0).await; // set_auth = open (0)

        let mut i = SsidInfo {
            len: ssid.len() as _,
            ssid: [0; 32],
        };
        i.ssid[..ssid.len()].copy_from_slice(ssid.as_bytes());

        self.wait_for_join(i).await;
    }

    pub async fn join_wpa2(&mut self, ssid: &str, passphrase: &str) {
        self.set_iovar_u32("ampdu_ba_wsize", 8).await;

        self.ioctl_set_u32(134, 0, 4).await; // wsec = wpa2
        self.set_iovar_u32x2("bsscfg:sup_wpa", 0, 1).await;
        self.set_iovar_u32x2("bsscfg:sup_wpa2_eapver", 0, 0xFFFF_FFFF).await;
        self.set_iovar_u32x2("bsscfg:sup_wpa_tmo", 0, 2500).await;

        Timer::after(Duration::from_millis(100)).await;

        let mut pfi = PassphraseInfo {
            len: passphrase.len() as _,
            flags: 1,
            passphrase: [0; 64],
        };
        pfi.passphrase[..passphrase.len()].copy_from_slice(passphrase.as_bytes());
        self.ioctl(IoctlType::Set, IOCTL_CMD_SET_PASSPHRASE, 0, &mut pfi.to_bytes())
            .await; // WLC_SET_WSEC_PMK

        self.ioctl_set_u32(20, 0, 1).await; // set_infra = 1
        self.ioctl_set_u32(22, 0, 0).await; // set_auth = 0 (open)
        self.ioctl_set_u32(165, 0, 0x80).await; // set_wpa_auth

        let mut i = SsidInfo {
            len: ssid.len() as _,
            ssid: [0; 32],
        };
        i.ssid[..ssid.len()].copy_from_slice(ssid.as_bytes());

        self.wait_for_join(i).await;
    }

    async fn wait_for_join(&mut self, i: SsidInfo) {
        let mut subscriber = self.event_sub.subscriber().unwrap();
        self.ioctl(IoctlType::Set, IOCTL_CMD_SET_SSID, 0, &mut i.to_bytes())
            .await;
        // set_ssid

        loop {
            let msg = subscriber.next_message_pure().await;
            if msg.event_type == Event::AUTH && msg.status != 0 {
                // retry
                warn!("JOIN failed with status={}", msg.status);
                self.ioctl(IoctlType::Set, IOCTL_CMD_SET_SSID, 0, &mut i.to_bytes())
                    .await;
            } else if msg.event_type == Event::JOIN && msg.status == 0 {
                // successful join
                break;
            }
        }

        self.state_ch.set_link_state(LinkState::Up);
        info!("JOINED");
    }

    pub async fn gpio_set(&mut self, gpio_n: u8, gpio_en: bool) {
        assert!(gpio_n < 3);
        self.set_iovar_u32x2("gpioout", 1 << gpio_n, if gpio_en { 1 << gpio_n } else { 0 })
            .await
    }

    async fn set_iovar_u32x2(&mut self, name: &str, val1: u32, val2: u32) {
        let mut buf = [0; 8];
        buf[0..4].copy_from_slice(&val1.to_le_bytes());
        buf[4..8].copy_from_slice(&val2.to_le_bytes());
        self.set_iovar(name, &buf).await
    }

    async fn set_iovar_u32(&mut self, name: &str, val: u32) {
        self.set_iovar(name, &val.to_le_bytes()).await
    }

    async fn get_iovar_u32(&mut self, name: &str) -> u32 {
        let mut buf = [0; 4];
        let len = self.get_iovar(name, &mut buf).await;
        assert_eq!(len, 4);
        u32::from_le_bytes(buf)
    }

    async fn set_iovar(&mut self, name: &str, val: &[u8]) {
        self.set_iovar_v::<64>(name, val).await
    }

    async fn set_iovar_v<const BUFSIZE: usize>(&mut self, name: &str, val: &[u8]) {
        info!("set {} = {:02x}", name, Bytes(val));

        let mut buf = [0; BUFSIZE];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        buf[name.len()] = 0;
        buf[name.len() + 1..][..val.len()].copy_from_slice(val);

        let total_len = name.len() + 1 + val.len();
        self.ioctl(IoctlType::Set, IOCTL_CMD_SET_VAR, 0, &mut buf[..total_len])
            .await;
    }

    // TODO this is not really working, it always returns all zeros.
    async fn get_iovar(&mut self, name: &str, res: &mut [u8]) -> usize {
        info!("get {}", name);

        let mut buf = [0; 64];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        buf[name.len()] = 0;

        let total_len = max(name.len() + 1, res.len());
        let res_len = self
            .ioctl(IoctlType::Get, IOCTL_CMD_GET_VAR, 0, &mut buf[..total_len])
            .await;

        let out_len = min(res.len(), res_len);
        res[..out_len].copy_from_slice(&buf[..out_len]);
        out_len
    }

    async fn ioctl_set_u32(&mut self, cmd: u32, iface: u32, val: u32) {
        let mut buf = val.to_le_bytes();
        self.ioctl(IoctlType::Set, cmd, iface, &mut buf).await;
    }

    async fn ioctl(&mut self, kind: IoctlType, cmd: u32, iface: u32, buf: &mut [u8]) -> usize {
        struct CancelOnDrop<'a>(&'a IoctlState);

        impl CancelOnDrop<'_> {
            fn defuse(self) {
                core::mem::forget(self);
            }
        }

        impl Drop for CancelOnDrop<'_> {
            fn drop(&mut self) {
                self.0.cancel_ioctl();
            }
        }

        let ioctl = CancelOnDrop(self.ioctl_state);

        ioctl.0.do_ioctl(kind, cmd, iface, buf).await;
        let resp_len = ioctl.0.wait_complete().await;

        ioctl.defuse();

        resp_len
    }

    pub async fn scan(&mut self) {
        const SCANTYPE_PASSIVE: u8 = 1;

        let scan_params = ScanParams {
            version: 1,
            action: 1,
            sync_id: 1,
            ssid_len: 0,
            ssid: [0; 32],
            bssid: [0xff; 6],
            bss_type: 2,
            scan_type: SCANTYPE_PASSIVE,
            nprobes: !0,
            active_time: !0,
            passive_time: !0,
            home_time: !0,
            channel_num: 0,
            channel_list: [0; 1],
        };

        let mut subscriber = self.event_sub.subscriber().unwrap();
        self.set_iovar_v::<256>("escan", &scan_params.to_bytes()).await;

        loop {
            let msg = subscriber.next_message_pure().await;
            if msg.event_type == Event::ESCAN_RESULT && msg.status == 0 {
                break;
            }
        }
    }
}
