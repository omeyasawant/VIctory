//! Which host adapter the device runs on.
//!
//! `D3D12CreateDevice(None, …)` takes whatever DXGI calls the default
//! adapter. On a hybrid laptop that is the adapter driving the primary
//! output — the integrated GPU — and on a machine with a remote-desktop or
//! emulator display driver it can be an indirect display adapter that
//! cannot create a D3D12 device at all. The title needs shader model 6.6
//! and, for the world, mesh shaders (README), so taking the default
//! silently leaves the capable GPU unused, or fails with a bare `E_FAIL`
//! that names nothing.
//!
//! This enumerates every adapter, probes what each one can do, and reports
//! that list when none of them can run the title.

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;

/// Shader model 6.6, the lowest the translator emits.
pub(crate) const SM_6_6: i32 = D3D_SHADER_MODEL_6_6.0;

/// What one enumerated adapter offers, from its description, one device
/// creation and two feature queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    pub name: String,
    pub luid: [u32; 2],
    pub vendor: u32,
    /// `DedicatedVideoMemory`; 0 for adapters that have none of their own.
    pub dedicated_vram: u64,
    /// DXGI marks WARP and the other renderers that are not hardware.
    pub software: bool,
    /// A D3D12 device could be created on it.
    pub device: bool,
    /// `CheckFeatureSupport(SHADER_MODEL)`, or 0 when there is no device.
    pub shader_model: i32,
    /// `D3D12_OPTIONS7::MeshShaderTier` is not `NONE`.
    pub mesh_shaders: bool,
}

impl Facts {
    /// Whether the title could run here at all: shader model 6.6 is what the
    /// translator emits, and a device must exist to ask.
    pub(crate) fn runs_the_title(&self) -> bool {
        // Not a software adapter: `Microsoft Basic Render Driver` reports
        // SM 6.6 and mesh shaders too, and running the title on the CPU is
        // worse than failing here. WARP stays an explicit switch
        // (`VICTORY_D3D12_WARP`), not something this can fall into.
        !self.software && self.device && self.shader_model >= SM_6_6
    }
}

/// The adapter to use: the ones that can run the title, mesh shaders first
/// (the world needs them), then the most dedicated memory, then the lowest
/// LUID so that the choice is the same on every run. `None` when no adapter
/// can create an SM 6.6 device.
pub(crate) fn choose(facts: &[Facts]) -> Option<usize> {
    facts
        .iter()
        .enumerate()
        .filter(|(_, f)| f.runs_the_title())
        .min_by_key(|(_, f)| (!f.mesh_shaders, std::cmp::Reverse(f.dedicated_vram), f.luid))
        .map(|(i, _)| i)
}

/// One line per adapter, for an error the user has to act on.
pub(crate) fn report(facts: &[Facts]) -> String {
    let mut text = String::new();
    for f in facts {
        let model = if f.device {
            format!("SM {:#x}", f.shader_model)
        } else {
            "no D3D12 device".to_string()
        };
        text.push_str(&format!(
            "\n  {} [LUID {:08x}:{:08x}, vendor {:04x}, {} MiB, {}{}{}]",
            f.name,
            f.luid[1],
            f.luid[0],
            f.vendor,
            f.dedicated_vram / (1024 * 1024),
            model,
            if f.mesh_shaders { ", mesh" } else { "" },
            if f.software { ", software" } else { "" },
        ));
    }
    text
}

/// Every adapter, probed in enumeration order.
fn enumerate() -> Vec<(IDXGIAdapter1, Facts)> {
    let mut found = Vec::new();
    // SAFETY: plain DXGI factory and adapter queries; every out pointer
    // below belongs to a local that outlives the call.
    let factory: IDXGIFactory4 = match unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) } {
        Ok(factory) => factory,
        Err(e) => {
            tracing::warn!(error = %e, "no DXGI factory; no adapter can be listed");
            return found;
        }
    };
    let mut index = 0;
    loop {
        // SAFETY: `EnumAdapters1` returns a new adapter interface for the
        // index, or an error once the index is past the last adapter.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        found.push((adapter.clone(), probe(&adapter)));
        index += 1;
        // A machine cannot have more adapters than this; a driver that
        // keeps answering would otherwise loop forever.
        if index > 32 {
            tracing::warn!("more than 32 adapters; the rest are ignored");
            break;
        }
    }
    found
}

/// Probes one adapter: its description, whether a device can be created on
/// it, and what that device supports.
fn probe(adapter: &IDXGIAdapter1) -> Facts {
    // SAFETY: `GetDesc1` fills a description owned by this call.
    let desc = unsafe { adapter.GetDesc1() }.unwrap_or_default();
    let name = String::from_utf16_lossy(
        &desc.Description[..desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len())],
    );
    let mut facts = Facts {
        name,
        luid: [desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart as u32],
        vendor: desc.VendorId,
        dedicated_vram: desc.DedicatedVideoMemory as u64,
        software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        device: false,
        shader_model: 0,
        mesh_shaders: false,
    };
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: the adapter is a live DXGI adapter, and `device` is an out
    // pointer to a local `Option` that outlives the call.
    if unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_err() {
        return facts;
    }
    let Some(device) = device else {
        return facts;
    };
    facts.device = true;
    let mut sm = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_6,
    };
    // SAFETY: `sm` is the structure the query fills, sized by `size_of_val`.
    if unsafe {
        device.CheckFeatureSupport(
            D3D12_FEATURE_SHADER_MODEL,
            (&raw mut sm).cast(),
            size_of_val(&sm) as u32,
        )
    }
    .is_ok()
    {
        facts.shader_model = sm.HighestShaderModel.0;
    }
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    // SAFETY: `options` is the structure the query fills, sized by
    // `size_of_val`.
    if unsafe {
        device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS7,
            (&raw mut options).cast(),
            size_of_val(&options) as u32,
        )
    }
    .is_ok()
    {
        facts.mesh_shaders = options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED;
    }
    facts
}

/// Creates a device on the best adapter and returns it with that adapter's
/// facts. Fails with every adapter listed when none can run the title.
pub(crate) fn open() -> anyhow::Result<(ID3D12Device, Facts)> {
    let probed = enumerate();
    if probed.is_empty() {
        anyhow::bail!("DXGI lists no adapter at all");
    }
    let facts: Vec<Facts> = probed.iter().map(|(_, f)| f.clone()).collect();
    let Some(index) = choose(&facts) else {
        // Nothing can run the title: say so, and say what is there.
        anyhow::bail!(
            "no adapter can run the title (shader model {:#x} needed); adapters:{}",
            SM_6_6,
            report(&facts)
        );
    };
    let (adapter, _) = &probed[index];
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: the adapter is a live DXGI adapter from enumeration, and
    // `device` is an out pointer to a local `Option`.
    unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }?;
    let device = device.ok_or_else(|| anyhow::anyhow!("the adapter reported no device"))?;
    let facts = facts[index].clone();
    tracing::info!(
        adapter = %facts.name,
        shader_model = format_args!("{:#x}", facts.shader_model),
        mesh_shaders = facts.mesh_shaders,
        adapters = probed.len(),
        "host adapter chosen"
    );
    Ok((device, facts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: &str, luid: u32, vram: u64, sm: i32, mesh: bool) -> Facts {
        Facts {
            name: name.to_string(),
            luid: [luid, 0],
            vendor: 0,
            dedicated_vram: vram,
            software: false,
            device: true,
            shader_model: sm,
            mesh_shaders: mesh,
        }
    }

    #[test]
    fn the_integrated_gpu_does_not_win_on_its_position() {
        // The hybrid laptop this is for: the iGPU is listed first and would
        // be the default adapter; the discrete GPU can run the world.
        let adapters = [
            facts("Intel(R) Arc(TM) Graphics", 1, 0, SM_6_6, true),
            facts("NVIDIA GeForce RTX 5070 Ti", 2, 16 << 30, SM_6_6, true),
        ];
        assert_eq!(choose(&adapters), Some(1));
    }

    #[test]
    fn an_adapter_without_sm_6_6_is_not_chosen() {
        let adapters = [
            facts("Old card", 1, 8 << 30, 0x65, false),
            facts("New card", 2, 4 << 30, SM_6_6, true),
        ];
        assert_eq!(choose(&adapters), Some(1));
    }

    #[test]
    fn a_display_adapter_without_a_device_is_not_chosen() {
        let mut virtual_display = facts("MuMu Virtual Display Adapter", 0, 0, 0, false);
        virtual_display.device = false;
        let adapters = [
            virtual_display,
            facts("Intel(R) Arc(TM) Graphics", 1, 0, SM_6_6, true),
        ];
        assert_eq!(choose(&adapters), Some(1));
    }

    #[test]
    fn mesh_shaders_beat_more_memory() {
        // The world's draws are mesh dispatches: an adapter without them
        // renders the menus and nothing else, whatever its memory.
        let adapters = [
            facts("Big, no mesh", 1, 24 << 30, SM_6_6, false),
            facts("Small, mesh", 2, 8 << 30, SM_6_6, true),
        ];
        assert_eq!(choose(&adapters), Some(1));
    }

    #[test]
    fn the_same_machine_gives_the_same_answer() {
        // Enumeration order is the driver's, so the choice must not depend
        // on it: the tie between two equal adapters is broken by the LUID.
        let a = facts("A", 7, 8 << 30, SM_6_6, true);
        let b = facts("B", 3, 8 << 30, SM_6_6, true);
        let picked = |list: &[Facts]| choose(list).map(|i| list[i].name.clone());
        assert_eq!(
            picked(&[a.clone(), b.clone()]),
            picked(&[b.clone(), a.clone()])
        );
        assert_eq!(picked(&[a, b]), Some("B".to_string()));
    }

    #[test]
    fn a_software_adapter_is_never_chosen() {
        // `Microsoft Basic Render Driver` reports SM 6.6 and mesh shaders on
        // this machine; running the title on the CPU is not a fallback.
        let mut basic = facts("Microsoft Basic Render Driver", 1, 0, SM_6_6, true);
        basic.software = true;
        assert_eq!(choose(&[basic.clone()]), None);
        let arc = facts("Intel(R) Arc(TM) Graphics", 2, 128 << 20, SM_6_6, true);
        assert_eq!(choose(&[basic, arc]), Some(1));
    }

    #[test]
    fn nothing_usable_gives_none_and_the_report_names_every_adapter() {
        let mut display_only = facts("MuMu Virtual Display Adapter", 2, 0, 0, false);
        display_only.device = false;
        let adapters = [
            facts("Intel(R) Arc(TM) Graphics", 1, 0, 0x65, false),
            display_only,
        ];
        assert_eq!(choose(&adapters), None);
        let report = report(&adapters);
        assert!(report.contains("Intel(R) Arc(TM) Graphics"));
        assert!(report.contains("MuMu Virtual Display Adapter"));
        assert!(report.contains("SM 0x65"));
        assert!(report.contains("no D3D12 device"));
    }
}
