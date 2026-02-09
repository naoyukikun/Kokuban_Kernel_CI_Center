use anyhow::{Result, anyhow};
use chrono::Local;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ProjectConfig;
use crate::utils::{handle_notify, load_projects, run_cmd, run_cmd_with_env};

pub fn handle_build(project_key: String, branch: String, do_release: bool) -> Result<()> {
    let projects = load_projects()?;
    let proj_val = projects
        .get(&project_key)
        .ok_or_else(|| anyhow!("Project not found"))?;
    let proj: ProjectConfig = serde_json::from_value(proj_val.clone())?;

    let kernel_source_path = PathBuf::from("kernel_source");
    if !kernel_source_path.exists() {
        return Err(anyhow!("Kernel source not found at ./kernel_source"));
    }

    // 1. Toolchain Setup
    if let Some(urls) = &proj.toolchain_urls {
        let tc_download_dir = PathBuf::from("toolchain_download");

        if tc_download_dir.exists() {
            fs::remove_dir_all(&tc_download_dir)?;
        }
        fs::create_dir_all(&tc_download_dir)?;

        for url in urls {
            println!("Downloading toolchain: {}", url);
            run_cmd(&["wget", "-q", url], Some(&tc_download_dir), false)?;
        }

        println!("Extracting toolchain...");
        let extract_script = r#"
            set -e
            if ls *.tar.gz.[0-9]* 1> /dev/null 2>&1; then
                cat *.tar.gz.* | tar -zxf - --warning=no-unknown-keyword -C ..
            elif ls *part_aa* 1> /dev/null 2>&1 || ls *_aa.tar.gz 1> /dev/null 2>&1 || ls *.tar.gz.aa 1> /dev/null 2>&1; then
                cat *.tar.gz | tar -zxf - --warning=no-unknown-keyword -C ..
            elif ls *.tar.gz 1> /dev/null 2>&1; then
                for tarball in *.tar.gz; do
                    tar -zxf "$tarball" --warning=no-unknown-keyword -C ..
                done
            fi
        "#;

        run_cmd(
            &["bash", "-c", extract_script],
            Some(&tc_download_dir),
            false,
        )?;

        fs::remove_dir_all(tc_download_dir)?;
    }

    // 2. Prepare Environment Variables
    let toolchain_prefix = proj.toolchain_path_prefix.as_deref().unwrap_or("");
    let toolchain_base = env::current_dir()?.join(toolchain_prefix);

    let mut build_env = HashMap::new();
    let current_path = env::var("PATH").unwrap_or_default();

    let mut new_path = current_path.clone();

    if let Some(exports) = &proj.toolchain_path_exports {
        for export in exports {
            let p = toolchain_base.join(export);
            new_path = format!("{}:{}", p.display(), new_path);
        }
    } else if !toolchain_prefix.is_empty() {
        new_path = format!("{}:{}", toolchain_base.join("bin").display(), new_path);
    }

    build_env.insert("PATH".to_string(), new_path);
    build_env.insert("ARCH".to_string(), "arm64".to_string());
    build_env.insert("CLANG_TRIPLE".to_string(), "aarch64-linux-gnu-".to_string());
    build_env.insert(
        "CROSS_COMPILE".to_string(),
        "aarch64-linux-gnu-".to_string(),
    );
    build_env.insert(
        "CROSS_COMPILE_COMPAT".to_string(),
        "arm-linux-gnueabi-".to_string(),
    );

    if let Some(true) = proj.extra_host_env {
        let kbt = toolchain_base.join("kernel-build-tools/linux-x86");
        let sysroot = toolchain_base.join("gcc/linux-x86/host/x86_64-linux-glibc2.17-4.8/sysroot");

        build_env.insert(
            "LD_LIBRARY_PATH".to_string(),
            format!(
                "{}:{}/lib64",
                env::var("LD_LIBRARY_PATH").unwrap_or_default(),
                kbt.display()
            ),
        );

        let sysroot_flag = format!("--sysroot={} ", sysroot.display());
        let cflags = format!("-I{}/include ", kbt.display());
        let ldflags = format!(
            "-L {}/lib64 -fuse-ld=lld --rtlib=compiler-rt",
            kbt.display()
        );

        build_env.insert(
            "HOSTCFLAGS".to_string(),
            format!("{}{}", sysroot_flag, cflags),
        );
        build_env.insert(
            "HOSTLDFLAGS".to_string(),
            format!("{}{}", sysroot_flag, ldflags),
        );
    }

    // ---------------------------------------------------------------------
    // 3. KernelSU Integration (MODIFIED FOR WILDKSU)
    // ---------------------------------------------------------------------
    if branch == "wildksu" {
        println!("Starting WildKSU + SUSFS + Manual Hook Integration");

        // A. Install WildKSU
        // Note: Using 'main' as argument per your script logic (bash -s wild)
        // Adjust the setup script URL if needed (using WildKernels URL from your snippet)
        let wild_setup = "curl -LSs 'https://raw.githubusercontent.com/WildKernels/Wild_KSU/wild/kernel/setup.sh' | bash -s wild";
        run_cmd(&["bash", "-c", wild_setup], Some(&kernel_source_path), false)?;

        // B. Clone SUSFS (Using shallow clone depth=1)
        println!("   - Cloning SUSFS...");
        let susfs_url = "https://gitlab.com/simonpunk/susfs4ksu.git";
        let susfs_branch = "gki-android13-5.15"; // You can make this dynamic if needed
        run_cmd(
            &["git", "clone", "-b", susfs_branch, "--depth=1", susfs_url, "susfs4ksu"],
            Some(&kernel_source_path),
            false,
        )?;

        // C. Apply SUSFS Patches
        println!("   - Applying SUSFS patches...");
        
        // Copy patch files
        let cp_patch_cmd = format!("cp susfs4ksu/kernel_patches/50_add_susfs_in_{}.patch .", susfs_branch);
        run_cmd(&["bash", "-c", &cp_patch_cmd], Some(&kernel_source_path), false)?;
        
        // Copy fs files
        run_cmd(&["bash", "-c", "cp -rv susfs4ksu/kernel_patches/fs/* fs/"], Some(&kernel_source_path), false)?;
        
        // Copy include files
        run_cmd(&["bash", "-c", "cp -rv susfs4ksu/kernel_patches/include/linux/* include/linux/"], Some(&kernel_source_path), false)?;

        // Apply the main patch
        let patch_cmd = format!("patch -p1 --fuzz=3 < 50_add_susfs_in_{}.patch", susfs_branch);
        run_cmd(&["bash", "-c", &patch_cmd], Some(&kernel_source_path), false)?;

        // D. Apply Manual Hook 1.6
        println!("   - Applying Manual Hook v1.6...");
        let hook_url = "https://github.com/SukiSU-Ultra/SukiSU_patch/raw/83aa64b7548890bb1f2eff6c990c03a1802df27b/hooks/scope_min_manual_hooks_v1.6.patch";
        run_cmd(&["curl", "-L", "-o", "manual-hook.patch", hook_url], Some(&kernel_source_path), false)?;
        run_cmd(&["bash", "-c", "patch -p1 --fuzz=3 < manual-hook.patch"], Some(&kernel_source_path), false)?;

        // E. Fix Compilation Error in fs/namespace.c
        // PROBLEM: The patch applied to a wrong function (approx line 3808) where variables are missing.
        // SOLUTION: Remove the bad lines and inject the logic into 'copy_mnt_ns' where 'copy_flags' exists.
        println!("   - Relocating Manual Hook to correct function...");

        // 1. Delete the misplaced 'if (flags & CLONE_NEWNS)' line
        run_cmd(
            &["sed", "-i", "/if (flags & CLONE_NEWNS)/d", "fs/namespace.c"],
            Some(&kernel_source_path),
            false,
        )?;

        // 2. Delete the misplaced 'copy_flags |= ...' line
        run_cmd(
            &["sed", "-i", "/copy_flags |= CL_COPY_MNT_NS/d", "fs/namespace.c"],
            Some(&kernel_source_path),
            false,
        )?;

        // 3. Inject the logic into the CORRECT place (inside copy_mnt_ns)
        // We match a unique line known to be in copy_mnt_ns and append our logic using '&' (which means 'matched string').
        // We use \\& to escape the ampersand for sed.
        run_cmd(
            &[
                "sed", 
                "-i", 
                "s/copy_flags = CL_COPY_UNBINDABLE | CL_EXPIRE;/& if (flags \\& CLONE_NEWNS) copy_flags |= CL_COPY_MNT_NS;/", 
                "fs/namespace.c"
            ],
            Some(&kernel_source_path),
            false,
        )?;

        // F. Adjust Configs (Disable Kprobes, Disable SUS_SU)
        // We write to a temporary config fragment or append to defconfig
        // Since we run 'make defconfig' later, we should append to the arch defconfig OR
        // handle it in the .config step later. Here we append to defconfig as requested.
        let defconfig_path = kernel_source_path.join(format!("arch/arm64/configs/{}", proj.defconfig));
        
        // Check if defconfig exists before appending
        if defconfig_path.exists() {
             let mut file = fs::OpenOptions::new().append(true).open(&defconfig_path)?;
             use std::io::Write;
             writeln!(file, "CONFIG_KSU_KPROBES_HOOK=n")?;
             writeln!(file, "CONFIG_KSU_SUSFS_SUS_SU=n")?;
        } else {
            println!("⚠️ Warning: Defconfig not found at {:?}, skipping config append.", defconfig_path);
        }

    } else {
        // Standard Logic for other variants
        let setup_url = match branch.as_str() {
            "resukisu" => Some((
                "https://raw.githubusercontent.com/ReSukiSU/ReSukiSU/main/kernel/setup.sh",
                "builtin",
            )),
            "mksu" => Some((
                "https://raw.githubusercontent.com/5ec1cff/KernelSU/main/kernel/setup.sh",
                "-",
            )),
            "ksu" => Some((
                "https://raw.githubusercontent.com/tiann/KernelSU/main/kernel/setup.sh",
                "-",
            )),
            _ => None,
        };

        if let Some((url, arg)) = setup_url {
            println!("Installing KernelSU for {}", branch);
            let cmd = format!("curl -LSs '{}' | bash -s {}", url, arg);
            run_cmd(&["bash", "-c", &cmd], Some(&kernel_source_path), false)?;
        }
    }

    // 4. Retrieve Kernel Version
    println!("Extracting kernel version...");
    let kernel_version = run_cmd(&["make", "kernelversion"], Some(&kernel_source_path), true)?
        .unwrap_or_else(|| "unknown".to_string())
        .trim()
        .to_string();
    println!("Detected Kernel Version: {}", kernel_version);

    // 5. Construct Make Arguments
    let target_soc = project_key.split('_').nth(1).unwrap_or("unknown");
    let mut make_args = vec!["O=out", "ARCH=arm64", "LLVM=1", "LLVM_IAS=1"];

    let soc_arg = format!("TARGET_SOC={}", target_soc);
    make_args.push(&soc_arg);

    if run_cmd(&["which", "ccache"], None, false).is_ok() {
        build_env.insert("CC".to_string(), "ccache clang".to_string());
        build_env.insert("CXX".to_string(), "ccache clang++".to_string());
        build_env.insert(
            "CCACHE_DIR".to_string(),
            format!("{}/.ccache", env::current_dir()?.display()),
        );
        run_cmd(&["ccache", "-M", "5G"], None, false)?;
        make_args.push("CC=ccache clang");
    } else {
        make_args.push("CC=clang");
    }

    // 6. Make Defconfig
    let mut defconfig_cmd = vec!["make"];
    defconfig_cmd.extend_from_slice(&make_args);
    defconfig_cmd.push(&proj.defconfig);

    run_cmd_with_env(&defconfig_cmd, Some(&kernel_source_path), &build_env)?;

    // 7. Apply Security & Config Patches
    let mut disable_configs = vec![
        "UH",
        "RKP",
        "KDP",
        "SECURITY_DEFEX",
        "INTEGRITY",
        "FIVE",
        "TRIM_UNUSED_KSYMS",
    ];
    if let Some(disables) = &proj.disable_security {
        for d in disables {
            disable_configs.push(d);
        }
    }

    // For WildKSU Manual Hook, ensure we enable Manual Hook config in the final .config
    if branch == "wildksu" {
         disable_configs.push("KSU_KPROBES_HOOK"); // Ensure KPROBES is off
         disable_configs.push("KSU_SUSFS_SUS_SU"); // Ensure SUS_SU is off
         
         // We must ENABLE Manual Hook. The loop below disables, so we do enable separately.
         run_cmd(
            &["scripts/config", "--file", "out/.config", "-e", "CONFIG_KSU_MANUAL_HOOK"],
            Some(&kernel_source_path),
            false,
        )?;
        run_cmd(
            &["scripts/config", "--file", "out/.config", "-e", "CONFIG_SUSFS"],
            Some(&kernel_source_path),
            false,
        )?;
    }

    for config in disable_configs {
        run_cmd(
            &[
                "scripts/config",
                "--file",
                "out/.config",
                "--disable",
                config,
            ],
            Some(&kernel_source_path),
            false,
        )?;
    }

    if let Some(lto) = &proj.lto {
        if lto == "thin" {
            run_cmd(
                &[
                    "scripts/config",
                    "--file",
                    "out/.config",
                    "-e",
                    "LTO_CLANG_THIN",
                    "-d",
                    "LTO_CLANG_FULL",
                ],
                Some(&kernel_source_path),
                false,
            )?;
        } else if lto == "full" {
            run_cmd(
                &[
                    "scripts/config",
                    "--file",
                    "out/.config",
                    "-e",
                    "LTO_CLANG_FULL",
                    "-d",
                    "LTO_CLANG_THIN",
                ],
                Some(&kernel_source_path),
                false,
            )?;
        }
    }

    // 8. Handle Localversion
    let short_sha = run_cmd(
        &["git", "rev-parse", "--short", "HEAD"],
        Some(&kernel_source_path),
        true,
    )?
    .unwrap_or_else(|| "unknown".to_string());

    let variant_suffix = match branch.as_str() {
        "main" | "lkm" => "LKM".to_string(),
        "ksu" => "KSU".to_string(),
        "mksu" => "MKSU".to_string(),
        "resukisu" | "sukisuultra" => "ReSuki".to_string(),
        "wildksu" => "WildKSU".to_string(), // Added label for filename
        _ => branch.to_uppercase(),
    };

    let localversion = format!("{}-{}", proj.localversion_base, variant_suffix);

    if proj.version_method.as_deref().unwrap_or("param") == "file" {
        fs::write(
            kernel_source_path.join("localversion"),
            format!("{}-g{}", localversion, short_sha),
        )?;
    } else {
        make_args.push("LOCALVERSION=");
        build_env.insert("LOCALVERSION".to_string(), localversion.clone());
    }

    // 9. Build Kernel
    let threads = run_cmd(&["nproc"], None, true)?.unwrap().trim().to_string();
    let jobs = format!("-j{}", threads);

    let mut build_cmd = vec!["make", &jobs];
    build_cmd.extend_from_slice(&make_args);

    run_cmd_with_env(&build_cmd, Some(&kernel_source_path), &build_env)?;

    if proj.version_method.as_deref().unwrap_or("param") == "file" {
        fs::write(kernel_source_path.join("localversion"), "")?;
    }

    // 10. Package AnyKernel3
    let ak3_repo = proj
        .anykernel_repo
        .as_deref()
        .unwrap_or("https://github.com/YuzakiKokuban/AnyKernel3.git");
    let ak3_branch = proj.anykernel_branch.as_deref().unwrap_or("master");

    if Path::new("AnyKernel3").exists() {
        fs::remove_dir_all("AnyKernel3")?;
    }

    run_cmd(
        &["git", "clone", ak3_repo, "-b", ak3_branch, "AnyKernel3"],
        None,
        false,
    )?;

    let image_path = kernel_source_path.join("out/arch/arm64/boot/Image");
    if !image_path.exists() {
        return Err(anyhow!("Image not found at {:?}", image_path));
    }

    fs::copy(image_path, "AnyKernel3/Image")?;

    let date_str = Local::now().format("%Y%m%d-%H%M").to_string();
    let zip_prefix = proj.zip_name_prefix.as_deref().unwrap_or("Kernel");

    let clean_localversion = localversion.trim_start_matches('-');
    let final_zip_name = format!(
        "{}-{}-{}-{}.zip",
        zip_prefix, kernel_version, clean_localversion, date_str
    );

    run_cmd(
        &[
            "zip",
            "-r9",
            format!("../{}", final_zip_name).as_str(),
            ".",
            "-x",
            ".git*",
            "-x",
            ".github*",
            "-x",
            "README.md",
            "-x",
            "LICENSE",
            "-x",
            "*.gitignore",
            "-x",
            "patch_linux",
            "-x",
            "tools/boot.img.lz4",
            "-x",
            "tools/libmagiskboot.so",
        ],
        Some(Path::new("AnyKernel3")),
        false,
    )?;

    // 11. Release & Notify
    if do_release {
        let release_tag = format!("{}-{}-{}", zip_prefix, variant_suffix, date_str);
        let release_title = format!("{} {} Build ({})", zip_prefix, variant_suffix, date_str);

        if Path::new(&final_zip_name).exists() {
            run_cmd(
                &[
                    "gh",
                    "release",
                    "create",
                    &release_tag,
                    &final_zip_name,
                    "--repo",
                    &proj.repo,
                    "--title",
                    &release_title,
                    "--notes",
                    &format!(
                        "Automated build for {}\nKernel Version: {}",
                        branch, kernel_version
                    ),
                ],
                None,
                false,
            )?;

            handle_notify(release_tag)?;
        } else {
            return Err(anyhow!("Final zip not found"));
        }
    }

    Ok(())
}
