use std::{
    env::current_exe,
    ffi::OsString,
    io::{
        self,
        BufRead,
        BufReader,
        Write,
    },
    process::{
        Child,
        Command,
        Stdio,
        exit,
    },
    thread::sleep,
    time::Duration,
};
use thelio_io::{
    fan::FanCurve,
    Io,
};
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl,
        ServiceControlAccept,
        ServiceExitCode,
        ServiceState,
        ServiceStatus,
        ServiceType,
    },
    service_dispatcher,
    service_control_handler::{
        self,
        ServiceControlHandlerResult,
    },
};

/// Duration to wait between sensor polling requests.
const POLLING_DELAY: Duration = Duration::from_secs(1);
/// Duration to keep fans high after temperatures drop.
const SPIN_DOWN_DELAY: Duration = Duration::from_secs(3);

fn driver_loop(curve: &FanCurve, ios: &mut [Io], wrapper: &mut Child) -> io::Result<()> {
    let mut wrapper_in = wrapper.stdin.take().unwrap();
    let mut wrapper_out = BufReader::new(wrapper.stdout.take().unwrap());

    // Build a poor-man's ring buffer that will store reported temperatures for SPIN_DOWN_DELAY.
    // The intent is to set the fans' duty cycle based on the highest temperature in the ring
    // buffer, rather than the most recent, preventing the fans from decreasing in speed until
    // SPIN_DOWN_DELAY has elapsed.
    // TODO: When `Duration::as_secs_f32()` is stabilized as a const fn, this can be made const.
    let points_in_spin_down = (SPIN_DOWN_DELAY.as_secs_f32()/POLLING_DELAY.as_secs_f32()).ceil() as usize;
    let mut recent_temps = vec![0.0; points_in_spin_down];
    let mut recent_temps_i = 0; // ring buffer index

    loop {
        // Write a newline to the thelio-io_wrapper.exe process to unblock its `Console.ReadLine()`.
        wrapper_in.write_all(b"\n")?;
        let mut line = String::new();
        wrapper_out.read_line(&mut line)?;

        // This will be the highest temperature read from all available sensors.
        // TODO: Is it possible to report individual component temperatures, or does that require
        //       pre-motherboard configuration?
        let read_temp = line.trim().parse::<f64>().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                err
            )
        })?;

        recent_temps[recent_temps_i] = read_temp;
        recent_temps_i = (recent_temps_i + 1) % points_in_spin_down;

        let temp = recent_temps.iter().max_by(|a, b| a.partial_cmp(b).unwrap()).unwrap();

        if let Some(duty) = curve.get_duty((temp * 100.0) as i16) {
            for io in ios.iter_mut() {
                for device in &["CPUF", "INTF"] {
                    io.set_duty(device, duty).map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::Other,
                            err
                        )
                    })?;
                }
            }
        }

        sleep(POLLING_DELAY);
    }
}

fn driver() -> io::Result<()> {
    let smbios = smbioslib::table_load_from_device()?;

    let sys_vendor = smbios.find_map(
        |sys: smbioslib::SMBiosSystemInformation| sys.manufacturer()
    ).unwrap_or(String::new());

    let product_version = smbios.find_map(
        |sys: smbioslib::SMBiosSystemInformation| sys.version()
    ).unwrap_or(String::new());

    let curve = match (sys_vendor.as_str(), product_version.as_str()) {
        ("System76", "thelio-mira-r1" | "thelio-mira-r2") => {
            log::debug!("{} {} uses the 'standard_smooth' fan curve", sys_vendor, product_version);
            FanCurve::standard_smooth()
        },
        ("System76", "thelio-major-r1") => {
            log::debug!("{} {} uses threadripper2 fan curve", sys_vendor, product_version);
            FanCurve::threadripper2()
        },
        ("System76", "thelio-major-r2" | "thelio-major-r2.1" | "thelio-major-b1" | "thelio-major-b2"
                   | "thelio-major-b3" | "thelio-mega-r1" | "thelio-mega-r1.1" ) => {
            log::debug!("{} {} uses hedt fan curve", sys_vendor, product_version);
            FanCurve::hedt()
        },
        ("System76", "thelio-massive-b1") => {
            log::debug!("{} {} uses xeon fan curve", sys_vendor, product_version);
            FanCurve::xeon()
        },
        _ => return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "unsupported sys_vendor '{}' and product_version '{}'",
                sys_vendor,
                product_version
            )
        )),
    };

    let mut ios = Vec::new();
    for port_info in serialport::available_ports()? {
        match port_info.port_type {
            serialport::SerialPortType::UsbPort(usb_info) => {
                if usb_info.vid == 0x1209 && usb_info.pid == 0x1776 {
                    log::debug!("Thelio Io at {}", port_info.port_name);

                    let port = serialport::new(port_info.port_name, 115200)
                        .timeout(Duration::from_millis(1))
                        .open()?;

                    let mut io = Io::new(port, 1000);
                    io.reset().map_err(|err| io::Error::new(
                        io::ErrorKind::Other,
                        err
                    ))?;
                    ios.push(io);
                }
            },
            _ => (),
        }
    }

    if ios.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "failed to find any Thelio Io devices"
        ));
    }

    let bin_path = current_exe()?;
    let bin_dir = bin_path.parent().unwrap();
    let wrapper_path = bin_dir.join("thelio-io_wrapper.exe");
    let mut wrapper = Command::new(&wrapper_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let res = driver_loop(&curve, &mut ios, &mut wrapper);

    let _ = wrapper.kill();

    res
}

fn service_main(_args: Vec<OsString>) {
    // Windows event log
    winlog::init("System76 Thelio Io").expect("failed to initialize logging");

    // Handle service events
    let status_handle = service_control_handler::register("thelio-io", |event| -> ServiceControlHandlerResult {
        //TODO: handle stop event
        match event {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    }).expect("failed to register for service events");

    // Update service status
    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }).expect("failed to set service status");

    // Run driver
    if let Err(err) = driver() {
        log::error!("{}\n{:#?}", err, err);
        //TODO: set service status
        exit(1);
    }
}

define_windows_service!(ffi_service_main, service_main);

fn main() -> Result<(), windows_service::Error> {
    // Dispatch service
    service_dispatcher::start("thelio-io", ffi_service_main)?;
    Ok(())
}
