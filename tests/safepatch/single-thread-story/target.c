#include "support/runtime.h"

/*
 * 单线程发票归档服务。
 *
 * 输入分两步到达：先收到发票编号，再读取正文中的数量。
 * 网络代理、管道客户端等上游都可能分批发送这两部分数据。
 * 所有归档写入都在 archive_invoice() 返回之前完成，没有后台任务。
 */
static int receive_quantity(void)
{
    int character = receive_byte();

    if (character < '0' || character > '9')
    {
        return -1;
    }

    return character - '0';
}

/*
 * 维护补丁只修改这个普通 C 函数，让后续调用直接返回 0。
 * 返回 1 表示已经归档，0 表示拒绝，负数表示输入或写入失败。
 */
int archive_invoice(int invoice_id)
{
    int quantity;
    long written;
    char record[] = "invoice-001 total=000\n";

    if (console_write("BODY\n", 5) != 5)
    {
        return -1;
    }

    /* 此处是在等待正常的业务正文，不是补丁门禁或协作停车点。 */
    quantity = receive_quantity();

    if (quantity < 0)
    {
        return -1;
    }

    /* 简化票据格式：编号为一位数，每件价格为 100 元。 */
    record[10] = (char)('0' + invoice_id);
    record[18] = (char)('0' + quantity);

    written = archive_write(record, sizeof(record) - 1);

    if (written != sizeof(record) - 1)
    {
        return -1;
    }

    if (console_write("COMMITTED\n", 10) != 10)
    {
        return -1;
    }

    return 1;
}

int main(void)
{
    int command;
    int result;

    if (console_write("READY\n", 6) != 6)
    {
        return 1;
    }

    /* 主线程按序处理请求，没有 pthread、协程或异步归档队列。 */
    for (;;)
    {
        command = receive_byte();

        if (command == 'Q')
        {
            return 0;
        }

        if (command < '1' || command > '9')
        {
            return 2;
        }

        result = archive_invoice(command - '0');

        if (result < 0)
        {
            return 3;
        }

        if (result == 0)
        {
            if (console_write("DENIED\n", 7) != 7)
            {
                return 4;
            }
        }
        else
        {
            if (console_write("DONE\n", 5) != 5)
            {
                return 5;
            }
        }
    }
}
