// SPDX-License-Identifier: MPL-2.0

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <sys/syscall.h>
#include <errno.h>

#include "../../common/test.h"

#define TIMER_SIG SIGRTMIN

static volatile siginfo_t received_si;
static volatile int signal_received;

static void timer_signal_handler(int sig, siginfo_t *si, void *unused)
{
	(void)sig;
	(void)unused;
	memcpy((void *)&received_si, si, sizeof(siginfo_t));
	signal_received = 1;
}

FN_SETUP(setup_timer_signal_handler)
{
	struct sigaction sa;
	memset(&sa, 0, sizeof(sa));
	sa.sa_flags = SA_SIGINFO;
	sa.sa_sigaction = timer_signal_handler;
	sigemptyset(&sa.sa_mask);
	CHECK(sigaction(TIMER_SIG, &sa, NULL));
}
END_SETUP()

FN_TEST(timer_create_sigev_signal_populates_si_timerid)
{
	timer_t timerid;
	struct sigevent sev;
	struct itimerspec its;

	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_SIGNAL;
	sev.sigev_signo = TIMER_SIG;
	sev.sigev_value.sival_int = 42;

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));

	its.it_value.tv_sec = 0;
	its.it_value.tv_nsec = 10000000;
	its.it_interval.tv_sec = 0;
	its.it_interval.tv_nsec = 0;
	TEST_SUCC(timer_settime(timerid, 0, &its, NULL));

	signal_received = 0;
	while (!signal_received)
		usleep(1000);

	TEST_RES(received_si.si_signo, _ret == TIMER_SIG);
	TEST_RES(received_si.si_code, _ret == SI_TIMER);
	TEST_RES(received_si.si_int, _ret == 42);

	TEST_SUCC(timer_delete(timerid));
}
END_TEST()

FN_TEST(timer_create_default_signal_populates_si_timerid)
{
	timer_t timerid;
	struct itimerspec its;

	struct sigevent sev;
	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_SIGNAL;
	sev.sigev_signo = TIMER_SIG;
	sev.sigev_value.sival_int = 99;

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));

	its.it_value.tv_sec = 0;
	its.it_value.tv_nsec = 10000000;
	its.it_interval.tv_sec = 0;
	its.it_interval.tv_nsec = 0;
	TEST_SUCC(timer_settime(timerid, 0, &its, NULL));

	signal_received = 0;
	while (!signal_received)
		usleep(1000);

	TEST_RES(received_si.si_signo, _ret == TIMER_SIG);
	TEST_RES(received_si.si_code, _ret == SI_TIMER);
	TEST_RES(received_si.si_int, _ret == 99);

	TEST_SUCC(timer_delete(timerid));
}
END_TEST()

FN_TEST(timer_create_thread_id_signal_populates_si_timerid)
{
	timer_t timerid;
	struct sigevent sev;
	struct itimerspec its;

	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_THREAD_ID;
	sev.sigev_signo = TIMER_SIG;
	sev.sigev_value.sival_int = 77;
	sev._sigev_un._tid = syscall(SYS_gettid);

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));

	its.it_value.tv_sec = 0;
	its.it_value.tv_nsec = 10000000;
	its.it_interval.tv_sec = 0;
	its.it_interval.tv_nsec = 0;
	TEST_SUCC(timer_settime(timerid, 0, &its, NULL));

	signal_received = 0;
	while (!signal_received)
		usleep(1000);

	TEST_RES(received_si.si_signo, _ret == TIMER_SIG);
	TEST_RES(received_si.si_code, _ret == SI_TIMER);
	TEST_RES(received_si.si_int, _ret == 77);

	TEST_SUCC(timer_delete(timerid));
}
END_TEST()

FN_TEST(timer_getoverrun_returns_zero_initially)
{
	timer_t timerid;
	struct sigevent sev;

	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_SIGNAL;
	sev.sigev_signo = TIMER_SIG;
	sev.sigev_value.sival_int = 0;

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));

	TEST_RES(syscall(SYS_timer_getoverrun, timerid), _ret == 0);

	TEST_SUCC(timer_delete(timerid));
}
END_TEST()

FN_TEST(timer_getoverrun_invalid_timerid)
{
	TEST_ERRNO(syscall(SYS_timer_getoverrun, 99999), EINVAL);
}
END_TEST()

FN_TEST(timer_delete_invalid_timerid)
{
	TEST_ERRNO(timer_delete((timer_t)99999), EINVAL);
}
END_TEST()

FN_TEST(timer_create_multiple_timers)
{
	timer_t timerid1, timerid2, timerid3;
	struct sigevent sev;

	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_SIGNAL;
	sev.sigev_signo = TIMER_SIG;
	sev.sigev_value.sival_int = 1;

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid1));

	sev.sigev_value.sival_int = 2;
	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid2));

	sev.sigev_value.sival_int = 3;
	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid3));

	TEST_RES(syscall(SYS_timer_getoverrun, timerid1), _ret == 0);
	TEST_RES(syscall(SYS_timer_getoverrun, timerid2), _ret == 0);
	TEST_RES(syscall(SYS_timer_getoverrun, timerid3), _ret == 0);

	TEST_SUCC(timer_delete(timerid1));
	TEST_SUCC(timer_delete(timerid2));
	TEST_SUCC(timer_delete(timerid3));
}
END_TEST()

FN_TEST(timer_signal_carries_correct_si_value_ptr)
{
	timer_t timerid;
	struct sigevent sev;
	struct itimerspec its;
	int value_data = 12345;

	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_SIGNAL;
	sev.sigev_signo = TIMER_SIG;
	sev.sigev_value.sival_ptr = &value_data;

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));

	its.it_value.tv_sec = 0;
	its.it_value.tv_nsec = 10000000;
	its.it_interval.tv_sec = 0;
	its.it_interval.tv_nsec = 0;
	TEST_SUCC(timer_settime(timerid, 0, &its, NULL));

	signal_received = 0;
	while (!signal_received)
		usleep(1000);

	TEST_RES(received_si.si_signo, _ret == TIMER_SIG);
	TEST_RES(received_si.si_code, _ret == SI_TIMER);
	TEST_RES((long)received_si.si_value.sival_ptr,
		 _ret == (long)&value_data);

	TEST_SUCC(timer_delete(timerid));
}
END_TEST()

FN_TEST(timer_create_delete_reuse)
{
	timer_t timerid;
	struct sigevent sev;

	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_SIGNAL;
	sev.sigev_signo = TIMER_SIG;
	sev.sigev_value.sival_int = 0;

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));
	TEST_SUCC(timer_delete(timerid));

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));
	TEST_RES(syscall(SYS_timer_getoverrun, timerid), _ret == 0);
	TEST_SUCC(timer_delete(timerid));
}
END_TEST()

FN_TEST(timer_sigev_none_no_signal)
{
	timer_t timerid;
	struct sigevent sev;
	struct itimerspec its;

	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_NONE;

	TEST_SUCC(timer_create(CLOCK_REALTIME, &sev, &timerid));

	its.it_value.tv_sec = 0;
	its.it_value.tv_nsec = 5000000;
	its.it_interval.tv_sec = 0;
	its.it_interval.tv_nsec = 0;
	TEST_SUCC(timer_settime(timerid, 0, &its, NULL));

	signal_received = 0;
	usleep(50000);
	TEST_RES(signal_received, _ret == 0);

	TEST_SUCC(timer_delete(timerid));
}
END_TEST()
