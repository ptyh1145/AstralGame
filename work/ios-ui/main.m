// Astral Game —— iOS 原生界面（armv7 / iOS 8+）
//
// 为什么不是原版 Flutter：Flutter 的 iOS 引擎只有 arm64、最低 iOS 13，
// 在 iOS 8/armv7 上根本无法运行。所以这里用 UIKit 做一个精简界面，
// 联机核心复用已经跑通的 Rust（EasyTier + utun），通过 C ABI 调用。
//
// 界面元素：
//   房间名 / 密码 / 服务器地址 三个输入框
//   [连接] 按钮 + 状态标签 + 滚动日志
//   [复制日志] 按钮（方便把日志贴出来排查）

#import <UIKit/UIKit.h>
#include <time.h>
#include <stdlib.h>
#include <sys/time.h>

// ---------------------------------------------------------------- Rust C ABI

extern int astral_start(const char *room, const char *secret,
                        const char *ipv4, const char *peer);
extern int astral_stop(void);
extern unsigned char astral_state(void);
extern const char *astral_state_text(void);
extern const char *astral_log(void);
extern void astral_clear_log(void);

// -------------------------------------------- 老系统缺失符号（供 std/ring 用）
//
// clock_gettime：iOS 10 才有；std 的 Timespec::now 与 quanta 会调用它。
//                这里用 gettimeofday 等价实现。
// CCRandomGenerateBytes：std 的 HashMap 随机源；用 arc4random_buf 替代。

int clock_gettime(int clk_id, struct timespec *tp) {
    if (tp == NULL) return -1;
    // 只支持 REALTIME(0) / MONOTONIC(6)
    if (clk_id != 0 && clk_id != 6) return -1;
    struct timeval tv;
    if (gettimeofday(&tv, NULL) != 0) return -1;
    tp->tv_sec = tv.tv_sec;
    tp->tv_nsec = (long)tv.tv_usec * 1000;
    return 0;
}

int CCRandomGenerateBytes(void *bytes, size_t count) {
    if (bytes == NULL) return -1;
    arc4random_buf(bytes, count);
    return 0;
}

// ---------------------------------------------------------------- 界面

@interface AstralViewController : UIViewController <UITextFieldDelegate>
@property (nonatomic, strong) UITextField *roomField;
@property (nonatomic, strong) UITextField *secretField;
@property (nonatomic, strong) UITextField *peerField;
@property (nonatomic, strong) UIButton *connectButton;
@property (nonatomic, strong) UILabel *stateLabel;
@property (nonatomic, strong) UITextView *logView;
@property (nonatomic, strong) NSTimer *pollTimer;
@end

@implementation AstralViewController

- (void)viewDidLoad {
    [super viewDidLoad];
    self.title = @"Astral Game";
    self.view.backgroundColor = [UIColor colorWithWhite:0.96 alpha:1.0];

    CGFloat w = self.view.bounds.size.width;
    CGFloat y = 20;

    self.roomField = [self makeField:@"房间名" y:y]; y += 52;
    self.roomField.text = @"test";
    self.secretField = [self makeField:@"密码" y:y]; y += 52;
    self.secretField.text = @"123456";
    self.peerField = [self makeField:@"服务器 (tcp://ip:11010)" y:y]; y += 52;

    self.connectButton = [UIButton buttonWithType:UIButtonTypeSystem];
    self.connectButton.frame = CGRectMake(16, y, w - 32, 44);
    [self.connectButton setTitle:@"连接" forState:UIControlStateNormal];
    self.connectButton.titleLabel.font = [UIFont boldSystemFontOfSize:18];
    [self.connectButton addTarget:self action:@selector(toggleConnect)
                 forControlEvents:UIControlEventTouchUpInside];
    [self.view addSubview:self.connectButton];
    y += 56;

    self.stateLabel = [[UILabel alloc] initWithFrame:CGRectMake(16, y, w - 32, 24)];
    self.stateLabel.font = [UIFont systemFontOfSize:15];
    self.stateLabel.textColor = [UIColor darkGrayColor];
    self.stateLabel.text = @"状态：未连接";
    [self.view addSubview:self.stateLabel];
    y += 32;

    self.logView = [[UITextView alloc] initWithFrame:CGRectMake(16, y, w - 32,
                                    self.view.bounds.size.height - y - 60)];
    self.logView.editable = NO;
    self.logView.font = [UIFont fontWithName:@"Menlo" size:11] ?: [UIFont systemFontOfSize:11];
    self.logView.backgroundColor = [UIColor whiteColor];
    self.logView.text = @"";
    [self.view addSubview:self.logView];

    UIButton *copyBtn = [UIButton buttonWithType:UIButtonTypeSystem];
    copyBtn.frame = CGRectMake(w - 140, self.view.bounds.size.height - 44, 130, 32);
    [copyBtn setTitle:@"复制日志" forState:UIControlStateNormal];
    [copyBtn addTarget:self action:@selector(copyLog) forControlEvents:UIControlEventTouchUpInside];
    [self.view addSubview:copyBtn];

    // iOS 8 兼容：用 NSTimer 轮询，而不是 CADisplayLink / async
    self.pollTimer = [NSTimer scheduledTimerWithTimeInterval:0.5
                                                      target:self
                                                    selector:@selector(poll)
                                                    userInfo:nil
                                                     repeats:YES];
}

- (UITextField *)makeField:(NSString *)placeholder y:(CGFloat)y {
    CGFloat w = self.view.bounds.size.width;
    UITextField *f = [[UITextField alloc] initWithFrame:CGRectMake(16, y, w - 32, 40)];
    f.placeholder = placeholder;
    f.borderStyle = UITextBorderStyleRoundedRect;
    f.autocorrectionType = UITextAutocorrectionTypeNo;
    f.autocapitalizationType = UITextAutocapitalizationTypeNone;
    f.clearButtonMode = UITextFieldViewModeWhileEditing;
    f.returnKeyType = UIReturnKeyDone;
    f.delegate = self;
    f.text = @"";
    [self.view addSubview:f];
    return f;
}

- (BOOL)textFieldShouldReturn:(UITextField *)textField {
    [textField resignFirstResponder];
    return YES;
}

- (void)poll {
    unsigned char st = astral_state();
    const char *txt = astral_state_text();
    NSString *s = [NSString stringWithUTF8String:(txt ? txt : "?")];
    NSString *prefix = nil;
    switch (st) {
        case 0: prefix = @"未连接"; break;
        case 1: prefix = @"连接中…"; break;
        case 2: prefix = @"已连接"; break;
        case 3: prefix = @"出错"; break;
        default: prefix = @"?"; break;
    }
    self.stateLabel.text = [NSString stringWithFormat:@"状态：%@ (%@)", prefix, s];
    self.stateLabel.textColor = (st == 3) ? [UIColor redColor]
                                : (st == 2 ? [UIColor colorWithRed:0 green:0.5 blue:0 alpha:1]
                                           : [UIColor darkGrayColor]);

    [self.connectButton setTitle:(st == 2 ? @"断开" : @"连接") forState:UIControlStateNormal];

    const char *lg = astral_log();
    if (lg) {
        NSString *full = [NSString stringWithUTF8String:lg];
        if (full.length > 0 && ![full isEqualToString:self.logView.text]) {
            self.logView.text = full;
            [self.logView scrollRangeToVisible:NSMakeRange(full.length, 0)];
        }
    }
}

- (void)toggleConnect {
    [self.view endEditing:YES];
    if (astral_state() == 2) {
        astral_stop();
        return;
    }
    astral_clear_log();
    NSString *peer = self.peerField.text ?: @"";
    peer = [peer stringByTrimmingCharactersInSet:
            [NSCharacterSet whitespaceAndNewlineCharacterSet]];
    // ipv4 固定；EasyTier 侧不用 DHCP
    NSString *ipv4 = @"10.144.144.5/24";
    int rc = astral_start([self.roomField.text UTF8String],
                          [self.secretField.text UTF8String],
                          [ipv4 UTF8String],
                          [peer UTF8String]);
    if (rc != 0) {
        UIAlertView *a = [[UIAlertView alloc] initWithTitle:@"启动失败"
                                                    message:@"请看下方日志"
                                                   delegate:nil
                                          cancelButtonTitle:@"好"
                                          otherButtonTitles:nil];
        [a show];
    }
}

- (void)copyLog {
    const char *lg = astral_log();
    NSString *s = lg ? [NSString stringWithUTF8String:lg] : @"";
    [UIPasteboard generalPasteboard].string = s;
    self.stateLabel.text = @"日志已复制到剪贴板";
}

@end

@interface AppDelegate : UIResponder <UIApplicationDelegate>
@property (nonatomic, strong) UIWindow *window;
@end

@implementation AppDelegate

- (BOOL)application:(UIApplication *)application
        didFinishLaunchingWithOptions:(NSDictionary *)launchOptions {
    self.window = [[UIWindow alloc] initWithFrame:[[UIScreen mainScreen] bounds]];
    AstralViewController *vc = [[AstralViewController alloc] init];
    UINavigationController *nav = [[UINavigationController alloc] initWithRootViewController:vc];
    self.window.rootViewController = nav;
    [self.window makeKeyAndVisible];
    return YES;
}

@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        return UIApplicationMain(argc, argv, nil, @"AppDelegate");
    }
}
