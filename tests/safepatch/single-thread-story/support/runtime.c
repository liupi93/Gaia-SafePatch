#include "runtime.h"

/*
 * 复用现有受限 ELF 的通用启动模板。
 * 这里只建立启动根并在 main 返回后退出，不包含业务或补丁协调逻辑。
 * 业务函数及受补丁函数均由 C 编译器生成，没有手工布置指令。
 */
__asm__(
    ".text\n"
    ".global _start\n"
    ".type _start,@function\n"
    "_start:\n"
    "xor %ebp,%ebp\n"
    "call main\n"
    "mov %eax,%edi\n"
    "mov $60,%eax\n"
    "syscall\n"
    "ud2\n"
    ".size _start,.-_start\n"
);

/* 以下薄封装只负责调用 Linux 的读写接口，栈帧由 C 编译器生成。 */
long console_write(const char *text, unsigned long length)
{
    long result;

    __asm__ volatile(
        "mov $1, %%eax\n"
        "syscall\n"
        : "=a"(result)
        : "D"(1L), "S"(text), "d"(length)
        : "rcx", "r11", "memory"
    );

    return result;
}

long archive_write(const char *text, unsigned long length)
{
    long result;

    __asm__ volatile(
        "mov $1, %%eax\n"
        "syscall\n"
        : "=a"(result)
        : "D"(3L), "S"(text), "d"(length)
        : "rcx", "r11", "memory"
    );

    return result;
}

int receive_byte(void)
{
    unsigned char character;
    long result;

    /*
     * 只读一个字节到本函数的局部变量。检查器会验证实际生成的
     * 缓冲区地址、长度、文件描述符以及分支不能绕过参数设置。
     */
    __asm__ volatile(
        "lea %1, %%rsi\n"
        "mov $0, %%edi\n"
        "mov $1, %%edx\n"
        "mov $0, %%eax\n"
        "syscall\n"
        : "=a"(result), "=m"(character)
        :
        : "rdi", "rsi", "rdx", "rcx", "r11", "memory"
    );

    if (result != 1)
    {
        return -1;
    }

    return character;
}
