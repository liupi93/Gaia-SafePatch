/* Ordinary freestanding native program: no SafePatch ABI, gate, counters or
 * parking protocol. Logs are observations, not input to the runtime checker. */
typedef unsigned long u64;
struct timespec { long sec, ns; };
__asm__(".text\n.global _start\n.type _start,@function\n_start:\n"
        "xor %ebp,%ebp\ncall main\nmov %eax,%edi\nmov $60,%eax\nsyscall\nud2\n"
        ".size _start,.-_start\n");
__attribute__((noinline)) static void output(const char *s, u64 n) {
    __asm__ volatile("mov $1, %%eax; syscall" : : "D"(1L), "S"(s), "d"(n) : "rax", "rcx", "r11", "memory");
}
__attribute__((noinline)) static void delay(void) {
    struct timespec t = {0, 250000000};
    __asm__ volatile("xor %%esi, %%esi; mov $35, %%eax; syscall" : : "D"(&t) : "rax", "rsi", "rcx", "r11", "memory");
}
__attribute__((noinline)) int target_value(void) {
    output("OLD_START\n",10);
    delay();
    output("OLD_DONE\n",9);
    return 10;
}
int main(void) {
    output("READY\n",6);
    for (int i=0; i<10000; i++) {
        int result=target_value();
        if (result!=10 && result!=99) return 7;
        if (result==99) output("NEW\n",4);
        output("GAP\n",4);
        delay();
    }
    return 0;
}

#ifdef BAD_SYSCALL
__attribute__((used,noinline)) static void unsupported(void) {
    __asm__ volatile("mov $57, %%eax; syscall" : : : "rax", "rcx", "r11", "memory");
}
#endif
