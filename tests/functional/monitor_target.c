#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 5) return 2;
    prctl(PR_SET_NAME, argv[1], 0, 0, 0);
    /* Service mode waits until its PID has been mapped by Gaia's tracker. */
    if (argv[2][0] != '-') {
        int n=0;
        while (access(argv[2], F_OK)) { if (++n>600) return 3; usleep(100000); }
    }
    int fd=open(argv[3], O_RDONLY); if (fd>=0) close(fd);
    errno=0; int uid_result=setuid(getuid()), uid_errno=errno;
    struct sockaddr_in addr={.sin_family=AF_INET,.sin_port=htons(atoi(argv[4]))};
    inet_pton(AF_INET,"127.0.0.1",&addr.sin_addr);
    int sock=socket(AF_INET,SOCK_STREAM,0); errno=0;
    int bind_result=bind(sock,(void*)&addr,sizeof(addr)), bind_errno=errno; close(sock);
    sock=socket(AF_INET,SOCK_STREAM,0); errno=0;
    int connect_result=connect(sock,(void*)&addr,sizeof(addr)), connect_errno=errno; close(sock);
    printf("{\"pid\":%d,\"file_open\":%s,\"setuid_rc\":%d,\"setuid_errno\":%d,\"bind_rc\":%d,\"bind_errno\":%d,\"connect_rc\":%d,\"connect_errno\":%d}\n",
        getpid(),fd>=0?"true":"false",uid_result,uid_errno,bind_result,bind_errno,connect_result,connect_errno);
    fflush(stdout);
    if(argv[2][0]!='-') sleep(3);
    return 0;
}
