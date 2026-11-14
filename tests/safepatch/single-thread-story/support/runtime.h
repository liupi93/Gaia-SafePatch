#ifndef GAIA_STORY_RUNTIME_H
#define GAIA_STORY_RUNTIME_H

/*
 * 极小运行时接口，作用相当于本示例所需的一小部分 C 标准库。
 * 归档文件由启动者打开并作为文件描述符 3 传入。
 */
long console_write(const char *text, unsigned long length);
long archive_write(const char *text, unsigned long length);
int receive_byte(void);

#endif
