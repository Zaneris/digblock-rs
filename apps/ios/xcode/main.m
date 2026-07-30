// Thin ObjC entry point. All app logic lives in the Rust staticlib;
// winit's run_app invokes UIApplicationMain internally.
extern void digblock_main(void);

int main(int argc, char *argv[]) {
	(void)argc;
	(void)argv;
	digblock_main();
	return 0;
}
